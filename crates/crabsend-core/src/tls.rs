//! TLS setup for the transfer server.
//!
//! Peers authenticate each other by certificate fingerprint, not by a
//! certificate authority, so the server accepts any self-consistent
//! self-signed client certificate and exposes the peer's fingerprint to the
//! application layer where identity is actually decided.

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use rustls::DigitallySignedStruct;
use rustls::DistinguishedName;
use rustls::SignatureScheme;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::server::danger::ClientCertVerified;
use rustls::server::danger::ClientCertVerifier;

use crate::crypto::Identity;
use crate::crypto::verify_self_signed_cert;

/// Installs the process-wide crypto provider.
///
/// Called before any TLS object is built; repeated calls are harmless.
pub fn install_crypto_provider() {
    // `ring` is the provider every dependency in this workspace is configured
    // for; installing it once keeps client and server on the same one.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Accepts any time-valid self-signed client certificate.
///
/// Client certificates are optional so a browser can still reach the plain
/// pages; a certificate that *is* presented must verify.
#[derive(Debug)]
pub struct AnyClientCertVerifier {
    mandatory: bool,
}

impl AnyClientCertVerifier {
    pub fn new(mandatory: bool) -> Self {
        Self { mandatory }
    }
}

impl ClientCertVerifier for AnyClientCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CA is involved, so there is nothing to hint.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        verify_self_signed_cert(end_entity).map_err(|error| {
            tracing::warn!("rejecting client certificate: {error}");
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )
        })?;
        Ok(ClientCertVerified::assertion())
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

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        self.mandatory
    }
}

/// Builds the server-side TLS configuration.
///
/// `require_client_cert` decides whether a peer without a certificate may open
/// a connection at all; official peers always present one.
pub fn server_config(
    identity: &Identity,
    require_client_cert: bool,
) -> Result<rustls::ServerConfig> {
    let cert = CertificateDer::from(crate::crypto::cert_der_from_pem(&identity.cert_pem)?);
    let key = PrivateKeyDer::try_from(crate::crypto::pem_private_key(&identity.key_pem)?)
        .map_err(|e| anyhow::anyhow!("parsing the device private key: {e}"))?;

    let mut config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AnyClientCertVerifier::new(require_client_cert)))
        .with_single_cert(vec![cert], key)
        .context("building the TLS server configuration")?;
    // HTTP/1.1 is what every LocalSend peer speaks; advertising h2 would let a
    // client pick a protocol this server does not serve.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}
