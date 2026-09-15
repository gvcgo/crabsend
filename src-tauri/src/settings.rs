//! Persisted application settings.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use crabsend_core::model::DEFAULT_PORT;
use crabsend_core::model::DeviceType;
use serde::Deserialize;
use serde::Serialize;

/// Everything the user can configure.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub alias: String,
    pub device_model: Option<String>,
    pub device_type: DeviceType,
    pub port: u16,
    /// Serve HTTPS (and require peer certificates) instead of plain HTTP.
    pub encryption: bool,
    /// Where received files are written.
    pub download_dir: PathBuf,
    /// PIN a sender must supply. `None` disables the check.
    pub pin: Option<String>,
    /// Accept incoming transfers without asking.
    pub auto_accept: bool,
    /// Compute file checksums before sending so the receiver can verify them.
    pub create_checksums: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            alias: default_alias(),
            device_model: device_model(),
            device_type: DeviceType::Desktop,
            port: DEFAULT_PORT,
            encryption: true,
            download_dir: default_download_dir(),
            pin: None,
            auto_accept: false,
            create_checksums: true,
        }
    }
}

impl Settings {
    /// Reads the settings file, falling back to defaults when it is missing or
    /// unreadable so a broken file never blocks startup.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str::<Settings>(&contents) {
                Ok(settings) => settings,
                Err(error) => {
                    tracing::warn!(
                        "ignoring unreadable settings at {}: {error}",
                        path.display()
                    );
                    Settings::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(error) => {
                tracing::warn!("cannot read {}: {error}", path.display());
                Settings::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Clamps values a user could type in but that would break the server.
    pub fn normalized(mut self) -> Self {
        self.port = self.port.max(1);
        self.alias = self.alias.trim().to_string();
        if self.alias.is_empty() {
            self.alias = default_alias();
        }
        self.pin = self
            .pin
            .map(|pin| pin.trim().to_string())
            .filter(|pin| !pin.is_empty());
        self
    }

    /// Whether changing to `other` requires restarting the transfer server.
    pub fn needs_server_restart(&self, other: &Settings) -> bool {
        self.port != other.port
            || self.encryption != other.encryption
            || self.alias != other.alias
            || self.device_model != other.device_model
            || self.device_type != other.device_type
            || self.download_dir != other.download_dir
    }
}

/// The device name shown to peers.
fn default_alias() -> String {
    hostname().unwrap_or_else(|| "Crabsend".to_string())
}

fn hostname() -> Option<String> {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let hostname = hostname.trim().to_string();
    if hostname.is_empty() || hostname == "localhost" {
        return None;
    }
    Some(hostname)
}

/// The model shown next to the device name.
fn device_model() -> Option<String> {
    Some(format!(
        "{} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

/// Downloads/Crabsend, falling back to the home directory.
fn default_download_dir() -> PathBuf {
    let base = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Crabsend")
}

/// Splits a path that is inside a phone application's own storage into the
/// storage root and the package the directory belongs to, for both places
/// Android gives an application: `…/Android/data/<package>/files/…` and
/// `…/Android/media/<package>/…`.
///
/// Returns `None` for anything else, which is every path on the desktop.
fn app_storage(path: &Path) -> Option<(PathBuf, String)> {
    let text = path.to_string_lossy();
    let (root, rest) = text
        .split_once("/Android/data/")
        .or_else(|| text.split_once("/Android/media/"))?;
    if root.is_empty() {
        return None;
    }
    let package = rest.split('/').find(|part| !part.is_empty())?;
    Some((PathBuf::from(root), package.to_string()))
}

/// The phone's public download directory, `Download/Crabsend`.
///
/// It sits beside the directories Android keeps for an application rather than
/// inside them, which is what makes it the one place a file manager, the system
/// Files application and a USB connection all show. Android 11 and later let an
/// application create files there without holding any storage permission.
///
/// `download_dir` is the directory the platform hands out, the application's
/// own one; `None` comes back for a path that is not on a phone.
pub fn android_public_dir(download_dir: &Path) -> Option<PathBuf> {
    let (root, _) = app_storage(download_dir)?;
    Some(root.join("Download").join("Crabsend"))
}

/// The shared directory that belongs to the application itself.
///
/// Android gives an application two places of its own: the files directory
/// (`/storage/emulated/0/Android/data/<package>/files/…`), which no file manager
/// and no other application can see, and the media directory beside it
/// (`/storage/emulated/0/Android/media/<package>/…`), which needs no permission
/// either but is not hidden. Up to Android 10 the public download directory
/// needs a storage permission this application does not ask for, which leaves
/// the media directory as the only place received files can be found.
///
/// `None` for any path that is not on a phone, as for [`android_public_dir`].
pub fn android_media_dir(download_dir: &Path) -> Option<PathBuf> {
    let (root, package) = app_storage(download_dir)?;
    Some(
        root.join("Android")
            .join("media")
            .join(package)
            .join("Crabsend"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phones_own_directory_maps_to_the_public_download_directory() {
        for own in [
            "/storage/emulated/0/Android/data/dev.crabsend.app/files/Download",
            "/storage/emulated/0/Android/data/dev.crabsend.app/files/Download/Crabsend",
            "/storage/emulated/0/Android/media/dev.crabsend.app/Crabsend",
        ] {
            assert_eq!(
                android_public_dir(Path::new(own)),
                Some(PathBuf::from("/storage/emulated/0/Download/Crabsend")),
                "{own}"
            );
        }
        // A desktop path has no such directory, and a malformed one names none.
        assert_eq!(
            android_public_dir(Path::new("/home/me/Downloads/Crabsend")),
            None
        );
        assert_eq!(android_public_dir(Path::new("/Android/data/")), None);
    }

    #[test]
    fn a_phones_files_directory_maps_to_its_visible_twin() {
        assert_eq!(
            android_media_dir(Path::new(
                "/storage/emulated/0/Android/data/dev.crabsend.app/files/Download/Crabsend"
            )),
            Some(PathBuf::from(
                "/storage/emulated/0/Android/media/dev.crabsend.app/Crabsend"
            ))
        );
        // The mapping is stable: the media directory it produces maps to itself,
        // which keeps a settings file that already points there where it is.
        assert_eq!(
            android_media_dir(Path::new(
                "/storage/emulated/0/Android/media/dev.crabsend.app/Crabsend"
            )),
            Some(PathBuf::from(
                "/storage/emulated/0/Android/media/dev.crabsend.app/Crabsend"
            ))
        );
        // A desktop path has no hidden twin, and a malformed one names nothing.
        assert_eq!(
            android_media_dir(Path::new("/home/me/Downloads/Crabsend")),
            None
        );
        assert_eq!(android_media_dir(Path::new("/Android/data/")), None);
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(&dir.path().join("missing.json"));
        assert_eq!(settings.port, DEFAULT_PORT);
        assert!(settings.encryption);
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings {
            alias: "Desk".to_string(),
            pin: Some("1234".to_string()),
            port: 53400,
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.alias, "Desk");
        assert_eq!(loaded.pin.as_deref(), Some("1234"));
        assert_eq!(loaded.port, 53400);
    }

    #[test]
    fn a_corrupt_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Settings::load(&path).port, DEFAULT_PORT);
    }

    #[test]
    fn blank_values_are_normalized_away() {
        let settings = Settings {
            alias: "   ".to_string(),
            pin: Some("  ".to_string()),
            port: 0,
            ..Settings::default()
        }
        .normalized();
        assert!(!settings.alias.trim().is_empty());
        assert_eq!(settings.pin, None);
        assert_eq!(settings.port, 1);
    }

    #[test]
    fn only_server_visible_fields_force_a_restart() {
        let base = Settings::default();
        let mut other = base.clone();
        other.create_checksums = !base.create_checksums;
        other.auto_accept = !base.auto_accept;
        assert!(!base.needs_server_restart(&other));

        other.port = base.port + 1;
        assert!(base.needs_server_restart(&other));
    }
}
