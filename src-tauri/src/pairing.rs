//! QR pairing: what this device's QR code carries, and how a scanned payload
//! is validated.
//!
//! The payload is plain JSON: it stays readable when the scanner cannot be
//! used, and a later version can add fields without breaking older readers. It
//! is *not* part of the LocalSend protocol — v2.2 defines no pairing or QR
//! concept and no peer implementation scans QR codes — so the shape below is
//! Crabsend's own.
//!
//! What it buys: the scanning device learns the peer's certificate fingerprint
//! out of band, from a code that is only readable while the two devices are in
//! the same room. Discovery over multicast cannot do that; a peer there is
//! trusted because it answered on the address the probe used.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use crabsend_core::model::DeviceType;
use crabsend_core::model::ProtocolType;
use qrcode::EcLevel;
use qrcode::QrCode;
use qrcode::render::svg;
use serde::Deserialize;
use serde::Serialize;

/// Payload version this build writes and accepts.
pub const PAYLOAD_VERSION: u8 = 1;

/// Shown when the payload carries no name for the peer.
const NAMELESS: &str = "Unnamed device";

/// Everything a peer needs to reach this device and to prove it reached it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingPayload {
    /// Payload version; a newer one is refused rather than misread.
    pub v: u8,
    /// Transport the peer's server speaks.
    pub protocol: ProtocolType,
    /// Addresses to try, in order. A multi-homed host cannot know which of its
    /// own addresses a scanner can reach, so the scanner walks the list.
    pub addresses: Vec<String>,
    /// Port the peer's server is bound to.
    pub port: u16,
    /// Uppercase-hex SHA-256 of the peer's certificate: the identity to pin.
    pub fingerprint: String,
    /// Name to show until the peer answers with its own.
    pub alias: String,
    /// Display hints, used until the peer answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
}

impl PairingPayload {
    /// Parses and validates a scanned string.
    pub fn parse(text: &str) -> Result<Self> {
        let mut payload: Self = serde_json::from_str(text.trim())
            .context("this is not a Crabsend pairing code")?;
        payload.normalize()?;
        Ok(payload)
    }

    /// Serializes the payload into the string the QR encodes.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serializing the pairing payload")
    }

    /// Rejects what cannot lead to a connection and trims what is merely untidy.
    fn normalize(&mut self) -> Result<()> {
        if self.v != PAYLOAD_VERSION {
            bail!(
                "this pairing code was written by another version of Crabsend (v{}, this build reads v{PAYLOAD_VERSION})",
                self.v
            );
        }

        self.fingerprint = self.fingerprint.trim().to_ascii_uppercase();
        if self.fingerprint.len() != 64 || !self.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("the pairing code carries no usable device fingerprint");
        }

        if self.port == 0 {
            bail!("the pairing code carries no port");
        }

        // Brackets are how IPv6 literals are typed, never how they are dialed.
        self.addresses = self
            .addresses
            .drain(..)
            .map(|address| {
                address
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string()
            })
            .filter(|address| !address.is_empty())
            .collect();
        let mut seen = Vec::with_capacity(self.addresses.len());
        self.addresses.retain(|address| {
            if seen.contains(address) {
                false
            } else {
                seen.push(address.clone());
                true
            }
        });
        if self.addresses.is_empty() {
            bail!("the pairing code carries no address to connect to");
        }

        self.alias = self.alias.trim().to_string();
        if self.alias.is_empty() {
            self.alias = NAMELESS.to_string();
        }
        self.device_model = self
            .device_model
            .take()
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());

        Ok(())
    }
}

/// A pairing QR code and the string it encodes.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingQr {
    /// The exact string inside the code, for reading and copying by hand.
    pub payload: String,
    /// Standalone SVG document, ready to be displayed.
    pub svg: String,
}

/// Renders a payload as a QR code in SVG form.
///
/// The colors are the conventional dark-on-light regardless of the interface
/// theme: an inverted code is not something every scanner reads.
pub fn qr_svg(payload: &str) -> Result<String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)
        .context("the pairing payload does not fit in a QR code")?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(256, 256)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> PairingPayload {
        PairingPayload {
            v: PAYLOAD_VERSION,
            protocol: ProtocolType::Https,
            addresses: vec!["192.168.1.42".to_string()],
            port: 53317,
            fingerprint: "4BADDE53A7F7CDEEED93189FD898E02BF6B4806CA4C05DE0ACE08319B86552FA"
                .to_string(),
            alias: "Desk".to_string(),
            device_model: Some("Arch Linux".to_string()),
            device_type: Some(DeviceType::Desktop),
        }
    }

    #[test]
    fn a_payload_survives_the_round_trip() {
        let original = payload();
        let decoded = PairingPayload::parse(&original.to_json().unwrap()).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_lowercase_fingerprint_is_normalized() {
        let text = payload()
            .to_json()
            .unwrap()
            .replace("4BADDE53", "4badde53");
        assert_eq!(
            PairingPayload::parse(&text).unwrap().fingerprint,
            payload().fingerprint
        );
    }

    #[test]
    fn brackets_and_duplicates_are_stripped_from_the_addresses() {
        let mut input = payload();
        input.addresses = vec![
            "[fe80::1]".to_string(),
            " 192.168.1.42 ".to_string(),
            "192.168.1.42".to_string(),
            String::new(),
        ];
        let decoded = PairingPayload::parse(&input.to_json().unwrap()).unwrap();
        assert_eq!(decoded.addresses, vec!["fe80::1", "192.168.1.42"]);
    }

    #[test]
    fn an_unnamed_peer_still_gets_a_label() {
        let mut input = payload();
        input.alias = "   ".to_string();
        let decoded = PairingPayload::parse(&input.to_json().unwrap()).unwrap();
        assert_eq!(decoded.alias, NAMELESS);
    }

    #[test]
    fn unusable_payloads_are_rejected() {
        let mut newer = payload();
        newer.v = PAYLOAD_VERSION + 1;
        assert!(PairingPayload::parse(&newer.to_json().unwrap()).is_err());

        let mut unhashed = payload();
        unhashed.fingerprint = "not-a-fingerprint".to_string();
        assert!(PairingPayload::parse(&unhashed.to_json().unwrap()).is_err());

        let mut stripped = payload();
        stripped.fingerprint = String::new();
        assert!(PairingPayload::parse(&stripped.to_json().unwrap()).is_err());

        let mut portless = payload();
        portless.port = 0;
        assert!(PairingPayload::parse(&portless.to_json().unwrap()).is_err());

        let mut addressless = payload();
        addressless.addresses = vec![" ".to_string()];
        assert!(PairingPayload::parse(&addressless.to_json().unwrap()).is_err());

        assert!(PairingPayload::parse("https://example.com").is_err());
        assert!(PairingPayload::parse("").is_err());
    }

    #[test]
    fn the_qr_renders_as_svg() {
        let svg = qr_svg(&payload().to_json().unwrap()).unwrap();
        // A complete document, dark on light: an inverted code is not something
        // every scanner reads.
        assert!(svg.contains("<svg"));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("#000000"));
        assert!(svg.contains("#ffffff"));
    }
}
