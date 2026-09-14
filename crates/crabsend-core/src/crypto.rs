//! Device identity (self-signed certificate) and hashing helpers.
//!
//! A peer's identity is the uppercase-hex SHA-256 fingerprint of its
//! certificate in DER form — that is what LocalSend peers compare, so the
//! encoding here is part of the wire contract.

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use sha2::Digest;
use sha2::Sha256;

/// This device's long-lived certificate and private key.
#[derive(Clone)]
pub struct Identity {
    /// PEM-encoded certificate.
    pub cert_pem: String,
    /// PEM-encoded PKCS#8 private key.
    pub key_pem: String,
    /// Uppercase-hex SHA-256 of the certificate in DER form.
    pub fingerprint: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The private key must never reach a log.
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Generates a fresh RSA-2048 identity.
    ///
    /// RSA-2048 (rather than an elliptic curve) matches what LocalSend peers
    /// generate and are known to verify; key generation is slow enough that
    /// callers persist the result.
    pub fn generate() -> Result<Self> {
        use rsa::pkcs8::EncodePrivateKey;
        use rsa::pkcs8::LineEnding;

        let mut rng = rsa::rand_core::OsRng;
        let private_key = rsa::RsaPrivateKey::new(&mut rng, 2048)?;
        let key_pem = private_key.to_pkcs8_pem(LineEnding::LF)?.to_string();

        let key_pair = rcgen::KeyPair::try_from(private_key.to_pkcs8_der()?.as_bytes())
            .context("converting the RSA key for rcgen")?;
        let mut params = rcgen::CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "LocalSend User");
        let cert = params
            .self_signed(&key_pair)
            .context("signing the self-signed certificate")?;

        Ok(Self {
            fingerprint: fingerprint_from_cert_der(cert.der()),
            cert_pem: cert.pem(),
            key_pem,
        })
    }

    /// Rebuilds an identity from stored PEM blocks, recomputing the fingerprint
    /// so a tampered file cannot desynchronize the two.
    pub fn from_pem(cert_pem: String, key_pem: String) -> Result<Self> {
        let der = cert_der_from_pem(&cert_pem)?;
        let fingerprint = fingerprint_from_cert_der(&der);
        Ok(Self {
            cert_pem,
            key_pem,
            fingerprint,
        })
    }

    /// The certificate in DER form.
    pub fn cert_der(&self) -> Result<Vec<u8>> {
        cert_der_from_pem(&self.cert_pem)
    }
}

/// Extracts the DER bytes of the first certificate in a PEM document.
pub fn cert_der_from_pem(pem: &str) -> Result<Vec<u8>> {
    decode_pem_block(pem, "CERTIFICATE")
}

/// Extracts the DER bytes of the first private key in a PEM document.
pub fn pem_private_key(pem: &str) -> Result<Vec<u8>> {
    let tag = pem
        .lines()
        .find_map(|line| line.strip_prefix("-----BEGIN ")?.strip_suffix("-----"))
        .filter(|tag| tag.ends_with("PRIVATE KEY"))
        .ok_or_else(|| anyhow::anyhow!("no private key in PEM document"))?;
    decode_pem_block(pem, tag)
}

/// Decodes the first `tag` block of a PEM document.
fn decode_pem_block(pem: &str, tag: &str) -> Result<Vec<u8>> {
    let begin = format!("-----BEGIN {tag}-----");
    let end = format!("-----END {tag}-----");
    let body: String = pem
        .lines()
        .skip_while(|line| line.trim() != begin)
        .skip(1)
        .take_while(|line| line.trim() != end)
        .collect();
    anyhow::ensure!(!body.is_empty(), "no {tag} block in PEM document");
    Ok(base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        body,
    )?)
}

/// Uppercase-hex SHA-256 of a DER certificate: the fingerprint peers exchange.
pub fn fingerprint_from_cert_der(cert: &[u8]) -> String {
    let digest = Sha256::digest(cert);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// Lowercase-hex SHA-256 of `data`, the format `FileDto.sha256` uses.
pub fn sha256_hex_bytes(data: &[u8]) -> String {
    hex_lower(&Sha256::digest(data))
}

/// Lowercase-hex SHA-256 of a file, read in 512 KiB chunks.
pub fn sha256_hex_file(path: &Path) -> Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 512 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A fresh opaque identifier for sessions, files and tokens.
pub fn random_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A random fingerprint for peers that announce over plain HTTP, where no
/// certificate exists to derive one from.
pub fn random_fingerprint() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Checks a peer certificate the way LocalSend does: it must be time-valid and
/// self-signed. There is no chain of trust — the fingerprint is the identity.
pub fn verify_self_signed_cert(der: &[u8]) -> Result<()> {
    use x509_parser::prelude::FromDer;

    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der)
        .map_err(|e| anyhow::anyhow!("parsing certificate: {e}"))?;
    anyhow::ensure!(
        cert.validity().is_valid(),
        "certificate outside its validity period"
    );
    cert.verify_signature(None)
        .map_err(|e| anyhow::anyhow!("certificate signature verification failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identity_is_self_consistent() {
        let identity = Identity::generate().unwrap();
        let der = identity.cert_der().unwrap();
        assert_eq!(identity.fingerprint, fingerprint_from_cert_der(&der));
        assert_eq!(identity.fingerprint.len(), 64);
        assert!(
            identity
                .fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
            "fingerprints are uppercase hex"
        );
        verify_self_signed_cert(&der).unwrap();
        // Round-tripping through the stored PEMs keeps the fingerprint stable.
        let reloaded =
            Identity::from_pem(identity.cert_pem.clone(), identity.key_pem.clone()).unwrap();
        assert_eq!(reloaded.fingerprint, identity.fingerprint);
    }

    #[test]
    fn fingerprint_matches_the_reference_vector() {
        // Certificate and expected fingerprint taken from the LocalSend core
        // test suite: the format must stay byte-identical to remain
        // interoperable.
        let pem = "-----BEGIN CERTIFICATE-----
MIIDGTCCAgGgAwIBAgIBATANBgkqhkiG9w0BAQsFADBQMRcwFQYDVQQDEw5Mb2Nh
bFNlbmQgVXNlcjEJMAcGA1UEChMAMQkwBwYDVQQLEwAxCTAHBgNVBAcTADEJMAcG
A1UECBMAMQkwBwYDVQQGEwAwHhcNMjUwMjA5MDAwMzE0WhcNMzUwMjA3MDAwMzE0
WjBQMRcwFQYDVQQDEw5Mb2NhbFNlbmQgVXNlcjEJMAcGA1UEChMAMQkwBwYDVQQL
EwAxCTAHBgNVBAcTADEJMAcGA1UECBMAMQkwBwYDVQQGEwAwggEiMA0GCSqGSIb3
DQEBAQUAA4IBDwAwggEKAoIBAQCL24MxhGfrdJm0Q8ZGiBkZ27ldcEChB4w7rSbJ
yiKeosoNbJl2kyj5dZjfBhWGgDLGDMM5w+Mh/5SrWgTL/QrhbB+lsrxILLznWqBi
R8wJP0P2YW9fBahQskJQcUXt/3jsCsMTWea4rWc3HZGh03bAkJfLM+PDSOfTpvAZ
6DQSp9QLzC9bgVNnq3W0SvOZGpF0xRa4InCyTUgxsNsV4+GIrmN5w4EbRFVVYu7D
5OS5fxNSCukiS0fb6oQzUp0vIAycvvWHHbAy8T6UMoUor2nfvNcryiaOX5WBMLyh
yMZ5gMOyXjdm1bT1XSlvtXPYUzxvsGAzTqS8mXjw8h7mm5htAgMBAAEwDQYJKoZI
hvcNAQELBQADggEBABZ+I7D6wkeSrsi1NBLP2zoZ5oGh+INNcGTravfOQHs4Fbas
/CysaUYjsD3fmaDh4MxgWEqAmWnnBiojfpGX2SGuFqRBKyT9DgitBt0L7Ezg1k3h
bfSiFW4hXWp75grVO8xfML7ZcWMlhKrOsOMUGiy1qs3qsyJ3w7B2Tz78HhXGO5dd
jyPmZarhixKO92UpEvKGxjO0E/3UUNUzxKTAAgFfhKpuwHUgIijM/EppZtA8OcSh
fEztiV0xKfcPVx4d6dqRt/NMElK1Ivw2vUuxTymphZkkFOzht9m73/kyKaeFp8Ij
VRus1zGVD8IVpIdPMyz01WJyS7M0fWaHXKWo+Bo=
-----END CERTIFICATE-----";
        assert_eq!(
            fingerprint_from_cert_der(&cert_der_from_pem(pem).unwrap()),
            "4BADDE53A7F7CDEEED93189FD898E02BF6B4806CA4C05DE0ACE08319B86552FA"
        );
    }

    #[test]
    fn sha256_hex_matches_known_digest() {
        assert_eq!(
            sha256_hex_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
