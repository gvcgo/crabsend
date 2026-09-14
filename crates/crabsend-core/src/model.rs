//! Wire types of the LocalSend v2 protocol.
//!
//! Field names, optionality and enum encodings are load-bearing: they are the
//! contract with official LocalSend peers, so every struct here maps 1:1 onto
//! the JSON described in the protocol specification (v2.2, plus the
//! `announce` flag of v2.0 that the multicast message still carries).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;

use serde::{Deserialize, Serialize};

/// Protocol version announced to peers.
pub const PROTOCOL_VERSION: &str = "2.2";

/// Default TCP (HTTP API) and UDP (multicast) port.
pub const DEFAULT_PORT: u16 = 53317;

/// Multicast group; inside `224.0.0.0/24` because some Android devices only
/// accept that range.
pub const DEFAULT_MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);

/// Link-local scoped IPv6 extension of the multicast group.
pub const DEFAULT_MULTICAST_GROUP_V6: Ipv6Addr =
    Ipv6Addr::new(0xff12, 0, 0, 0, 0, 0, 0xfd3a, 0xe420);

/// Kind of device, for UI purposes only.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Mobile,
    #[default]
    Desktop,
    Web,
    Headless,
    Server,
}

impl DeviceType {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceType::Mobile => "mobile",
            DeviceType::Desktop => "desktop",
            DeviceType::Web => "web",
            DeviceType::Headless => "headless",
            DeviceType::Server => "server",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "mobile" => DeviceType::Mobile,
            "web" => DeviceType::Web,
            "headless" => DeviceType::Headless,
            "server" => DeviceType::Server,
            // The protocol requires unknown values to fall back to desktop.
            _ => DeviceType::Desktop,
        }
    }

    pub const ALL: [DeviceType; 5] = [
        DeviceType::Mobile,
        DeviceType::Desktop,
        DeviceType::Web,
        DeviceType::Headless,
        DeviceType::Server,
    ];
}

/// Transport the peer's API is reachable on.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    Http,
    #[default]
    Https,
}

impl ProtocolType {
    pub fn as_str(self) -> &'static str {
        match self {
            ProtocolType::Http => "http",
            ProtocolType::Https => "https",
        }
    }
}

/// This device's own identity as announced to peers.
///
/// Used as the `info` object of `prepare-upload` and as the request body of
/// `register`, where `port` and `protocol` describe *this* device's server.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterDto {
    pub alias: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
    pub fingerprint: String,
    pub port: u16,
    pub protocol: ProtocolType,
    #[serde(default)]
    pub download: bool,
}

/// Another device's identity as returned by `register` / `info`.
///
/// Deliberately has no `port`/`protocol`: peers are dialed on the address and
/// port they were discovered on, never on values echoed back by a response.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfoDto {
    pub alias: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub download: bool,
}

/// Multicast announcement.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MulticastMessage {
    pub alias: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
    pub fingerprint: String,
    pub port: u16,
    pub protocol: ProtocolType,
    #[serde(default)]
    pub download: bool,
    /// Always `true` for announcements; kept for v2.0 peers.
    #[serde(default = "default_true")]
    pub announce: bool,
}

fn default_true() -> bool {
    true
}

/// Metadata of one file offered for transfer.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDto {
    pub id: String,
    pub file_name: String,
    pub size: u64,
    pub file_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<FileMetadata>,
}

/// Source timestamps, RFC 3339.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FileMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadRequest {
    pub info: RegisterDto,
    pub files: BTreeMap<String, FileDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadResponse {
    pub session_id: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareDownloadResponse {
    pub info: DeviceInfoDto,
    pub session_id: String,
    pub files: BTreeMap<String, FileDto>,
}

/// Error body of every non-2xx response the API produces.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorResponse {
    pub message: String,
}

impl ErrorResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_type_uses_lowercase_wire_values() {
        let json = serde_json::to_string(&DeviceType::Headless).unwrap();
        assert_eq!(json, "\"headless\"");
        assert_eq!(
            serde_json::from_str::<DeviceType>("\"server\"").unwrap(),
            DeviceType::Server
        );
    }

    #[test]
    fn unknown_device_type_falls_back_to_desktop() {
        assert_eq!(DeviceType::parse("fridge"), DeviceType::Desktop);
    }

    #[test]
    fn register_dto_roundtrips_camel_case() {
        let dto = RegisterDto {
            alias: "Nice Orange".into(),
            version: PROTOCOL_VERSION.into(),
            device_model: Some("Linux".into()),
            device_type: Some(DeviceType::Desktop),
            fingerprint: "ABC".into(),
            port: 53317,
            protocol: ProtocolType::Https,
            download: true,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["deviceModel"], "Linux");
        assert_eq!(json["deviceType"], "desktop");
        assert_eq!(json["protocol"], "https");
        let parsed: RegisterDto = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.port, 53317);
        assert_eq!(parsed.protocol, ProtocolType::Https);
    }

    #[test]
    fn device_info_response_parses_without_optional_fields() {
        let parsed: DeviceInfoDto = serde_json::from_str(
            r#"{"alias":"Secret Banana","version":"2.0","fingerprint":"abc"}"#,
        )
        .unwrap();
        assert_eq!(parsed.device_model, None);
        assert!(!parsed.download);
    }

    #[test]
    fn multicast_message_defaults_announce_to_true() {
        let parsed: MulticastMessage = serde_json::from_str(
            r#"{"alias":"a","version":"2.2","fingerprint":"f","port":1,"protocol":"http"}"#,
        )
        .unwrap();
        assert!(parsed.announce);
        assert_eq!(parsed.protocol, ProtocolType::Http);
    }
}
