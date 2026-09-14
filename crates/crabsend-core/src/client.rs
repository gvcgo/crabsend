//! HTTP client for the sender side of the protocol.
//!
//! Every request is made through a client that is pinned to the peer's
//! fingerprint, so file content can never be streamed to a different peer than
//! the one the user picked, even if the network redirects the connection.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use futures_util::StreamExt;
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::crypto::Identity;
use crate::crypto::fingerprint_from_cert_der;
use crate::crypto::verify_self_signed_cert;
use crate::model::DeviceInfoDto;
use crate::model::ErrorResponse;
use crate::model::PrepareUploadRequest;
use crate::model::PrepareUploadResponse;
use crate::model::ProtocolType;
use crate::model::RegisterDto;

/// Timeout used for discovery probes, where a silent host must not stall the
/// scan.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a TCP connection may take to come up before the peer counts as
/// unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a peer's API lives.
#[derive(Clone, Debug)]
pub struct HttpTarget {
    pub protocol: ProtocolType,
    pub host: String,
    pub port: u16,
}

impl HttpTarget {
    pub fn new(protocol: ProtocolType, host: impl Into<String>, port: u16) -> Self {
        Self {
            protocol,
            host: host.into(),
            port,
        }
    }

    /// Base URL of the peer's v2 API.
    pub fn base_url(&self) -> String {
        format!(
            "{}://{}:{}/api/localsend/v2",
            self.protocol.as_str(),
            encode_host(&self.host),
            self.port
        )
    }
}

/// What the receiver decided about a `prepare-upload` request.
#[derive(Clone, Debug)]
pub enum PrepareOutcome {
    /// `204 No Content`: the receiver wants none of the files.
    NothingToTransfer,
    /// `200 OK`: the files the receiver accepted, mapped to their tokens.
    Accepted {
        session_id: String,
        files: BTreeMap<String, String>,
    },
}

/// Failures a caller has to tell apart to drive the UI.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The local user aborted the request.
    #[error("cancelled")]
    Cancelled,
    /// The peer answered with a non-success status.
    #[error("peer responded with {status}{}", match .message { Some(message) => format!(": {message}"), None => String::new() })]
    Status {
        status: u16,
        message: Option<String>,
    },
    /// Anything else: DNS, TLS, I/O, malformed JSON.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ClientError {
    /// HTTP status of the failure, when there was one.
    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// A client bound to one peer and its identity.
pub struct HttpClient {
    client: reqwest::Client,
    base_url: String,
    protocol: ProtocolType,
}

impl HttpClient {
    /// Builds a client for `target`.
    ///
    /// `pin_fingerprint` is the peer's uppercase-hex certificate fingerprint;
    /// when given, a peer presenting any other certificate fails the TLS
    /// handshake. Pass `None` only where the peer is still being discovered.
    pub fn new(
        identity: &Identity,
        target: &HttpTarget,
        pin_fingerprint: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            // Local peers must never be reached through a proxy, and a
            // redirect would move the request off the pinned connection.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .dns_resolver(Arc::new(ScopedHostResolver));

        if target.protocol == ProtocolType::Https {
            let tls = build_tls_config(identity, pin_fingerprint)?;
            builder = builder.tls_backend_preconfigured(tls).tls_info(true);
        }
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }

        Ok(Self {
            client: builder.build().context("building the HTTP client")?,
            base_url: target.base_url(),
            protocol: target.protocol,
        })
    }

    /// Announces this device to the peer and returns the peer's own info.
    pub async fn register(&self, info: &RegisterDto) -> Result<DeviceInfoDto, ClientError> {
        let url = format!("{}/register", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(info)
            .send()
            .await
            .map_err(reqwest_error)?;
        self.json_body(response).await
    }

    /// Fetches the peer's info without registering; useful to probe a host.
    pub async fn info(&self) -> Result<DeviceInfoDto, ClientError> {
        let url = format!("{}/info", self.base_url);
        let response = self.client.get(&url).send().await.map_err(reqwest_error)?;
        self.json_body(response).await
    }

    /// Asks the peer whether it accepts `request`, and for upload tokens.
    pub async fn prepare_upload(
        &self,
        request: &PrepareUploadRequest,
        pin: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<PrepareOutcome, ClientError> {
        let mut url = format!("{}/prepare-upload", self.base_url);
        if let Some(pin) = pin {
            url.push_str(&format!("?pin={}", percent_encode(pin)));
        }
        let send = self.client.post(&url).json(request).send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ClientError::Cancelled),
            response = send => response.map_err(reqwest_error)?,
        };

        let status = response.status().as_u16();
        if status == 204 {
            return Ok(PrepareOutcome::NothingToTransfer);
        }
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        let body: PrepareUploadResponse = response.json().await.map_err(reqwest_error)?;
        Ok(PrepareOutcome::Accepted {
            session_id: body.session_id,
            files: body.files,
        })
    }

    /// Streams one file to the peer.
    ///
    /// `progress` receives the cumulative number of bytes sent.
    pub async fn upload(
        &self,
        session_id: &str,
        file_id: &str,
        token: &str,
        path: &Path,
        progress: impl Fn(u64) + Send + Sync + 'static,
        cancel: &CancellationToken,
    ) -> Result<(), ClientError> {
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {} for upload", path.display()))?;
        let transferred = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let progress = Arc::new(progress);
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file).map(move |chunk| {
            if let Ok(bytes) = &chunk {
                let total = transferred
                    .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed)
                    + bytes.len() as u64;
                progress(total);
            }
            chunk
        }));

        let url = format!(
            "{}/upload?sessionId={}&fileId={}&token={}",
            self.base_url,
            percent_encode(session_id),
            percent_encode(file_id),
            percent_encode(token)
        );
        let send = self.client.post(&url).body(body).send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ClientError::Cancelled),
            response = send => response.map_err(reqwest_error)?,
        };
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        Ok(())
    }

    /// Tells the peer that the session is over.
    ///
    /// Best effort: the peer may already have dropped it, which is not an error.
    pub async fn cancel(&self, session_id: &str) {
        let url = format!(
            "{}/cancel?sessionId={}",
            self.base_url,
            percent_encode(session_id)
        );
        if let Err(error) = self.client.post(&url).send().await {
            tracing::debug!("cancel request failed: {error}");
        }
    }

    /// The protocol this client speaks to the peer.
    pub fn protocol(&self) -> ProtocolType {
        self.protocol
    }

    async fn json_body<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, ClientError> {
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        response.json().await.map_err(reqwest_error)
    }
}

fn reqwest_error(error: reqwest::Error) -> ClientError {
    ClientError::Other(anyhow::Error::new(error))
}

/// Turns a non-success response into an error carrying the peer's message.
async fn status_error(response: reqwest::Response) -> ClientError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ErrorResponse>(&body)
        .ok()
        .map(|error| error.message)
        .or_else(|| {
            let trimmed = body.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
    ClientError::Status { status, message }
}

/// Percent-encodes a query parameter value, keeping the unreserved set.
///
/// Session ids and tokens are base64 or UUIDs; `+`, `/` and `=` must not be
/// read as query syntax by the receiver.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Renders a host for use in a URL: IPv6 needs brackets, and a scoped
/// link-local address cannot be expressed at all and is replaced by a
/// synthetic name that [`ScopedHostResolver`] maps back.
pub fn encode_host(host: &str) -> String {
    if let Some(encoded) = encode_scoped_host(host) {
        return encoded;
    }
    if host.contains(':') {
        return format!("[{host}]");
    }
    host.to_string()
}

/// Suffix of the synthetic names used for scoped IPv6 addresses.
const SCOPED_SUFFIX: &str = ".scoped.localsend.internal";

/// `fe80::1%3` -> `fe80--1s3.scoped.localsend.internal`.
fn encode_scoped_host(host: &str) -> Option<String> {
    let (address, scope) = host.split_once('%')?;
    let address: std::net::Ipv6Addr = address.parse().ok()?;
    let scope: u32 = scope.parse().ok()?;
    Some(format!(
        "{}s{scope}{SCOPED_SUFFIX}",
        address.to_string().replace(':', "-")
    ))
}

/// Maps the synthetic scoped names back to socket addresses; every other name
/// is resolved by the system resolver.
#[derive(Debug)]
struct ScopedHostResolver;

impl reqwest::dns::Resolve for ScopedHostResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().to_string();
        Box::pin(async move {
            if let Some(socket_addr) = decode_scoped_host(&name) {
                let addrs: reqwest::dns::Addrs = Box::new(std::iter::once(socket_addr));
                return Ok(addrs);
            }
            let addrs = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let addrs: Vec<std::net::SocketAddr> = addrs.collect();
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn decode_scoped_host(name: &str) -> Option<std::net::SocketAddr> {
    let stem = name.strip_suffix(SCOPED_SUFFIX)?;
    let (address, scope) = stem.rsplit_once('s')?;
    let address: std::net::Ipv6Addr = address.replace('-', ":").parse().ok()?;
    let scope: u32 = scope.parse().ok()?;
    // Port 0 is a placeholder; the client substitutes the URL's port.
    Some(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
        address, 0, 0, scope,
    )))
}

/// Builds the rustls configuration used for every peer connection: the device
/// certificate is presented, and the peer is trusted only if its certificate is
/// self-signed, time-valid and (when pinned) has the expected fingerprint.
fn build_tls_config(
    identity: &Identity,
    pin_fingerprint: Option<&str>,
) -> Result<rustls::ClientConfig> {
    let certs = vec![CertificateDer::from(crate::crypto::cert_der_from_pem(
        &identity.cert_pem,
    )?)];
    let key = PrivateKeyDer::try_from(pem_key_bytes(&identity.key_pem)?)
        .map_err(|e| anyhow::anyhow!("parsing the device private key: {e}"))?;

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(pin_fingerprint)))
        .with_client_auth_cert(certs, key)?;
    // HTTP/1.1 only: HTTP/2's flow-control window throttles bulk uploads, and
    // the reference implementation negotiates the same.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

fn pem_key_bytes(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .skip_while(|line| !line.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END"))
        .collect();
    anyhow::ensure!(!body.is_empty(), "no private key in PEM document");
    Ok(base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        body,
    )?)
}

/// Accepts any self-consistent peer certificate, and additionally requires a
/// fingerprint match when one is pinned.
#[derive(Debug)]
struct PinnedCertVerifier {
    expected: Option<String>,
}

impl PinnedCertVerifier {
    fn new(expected: Option<&str>) -> Self {
        Self {
            expected: expected.map(|value| value.to_ascii_uppercase()),
        }
    }

    fn check(&self, certificate: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        // Hostnames carry no meaning here: peers are addressed by IP and their
        // certificates have no SAN, so the fingerprint is the identity.
        verify_self_signed_cert(certificate).map_err(|error| {
            tracing::warn!("rejecting peer certificate: {error}");
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )
        })?;
        if let Some(expected) = &self.expected {
            let actual = fingerprint_from_cert_der(certificate);
            if &actual != expected {
                tracing::warn!("peer certificate fingerprint {actual} does not match {expected}");
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure,
                ));
            }
        }
        Ok(())
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_rendered_for_urls() {
        assert_eq!(encode_host("192.168.1.5"), "192.168.1.5");
        assert_eq!(encode_host("fe80::1"), "[fe80::1]");
        assert_eq!(
            encode_host("fe80::1%3"),
            "fe80--1s3.scoped.localsend.internal"
        );
    }

    #[test]
    fn scoped_hosts_round_trip() {
        let encoded = encode_host("fe80::a00:27ff:fe4e:66a1%7");
        let decoded = decode_scoped_host(&encoded).unwrap();
        match decoded {
            std::net::SocketAddr::V6(addr) => {
                assert_eq!(addr.ip().to_string(), "fe80::a00:27ff:fe4e:66a1");
                assert_eq!(addr.scope_id(), 7);
            }
            _ => panic!("expected an IPv6 socket address"),
        }
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(percent_encode("a+b/c="), "a%2Bb%2Fc%3D");
        assert_eq!(percent_encode("plain-value_1.0~"), "plain-value_1.0~");
    }

    #[test]
    fn base_url_uses_the_peer_protocol_and_port() {
        let target = HttpTarget::new(ProtocolType::Https, "10.0.0.2", 53317);
        assert_eq!(target.base_url(), "https://10.0.0.2:53317/api/localsend/v2");
    }
}
