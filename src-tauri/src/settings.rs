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
            device_type: device_type(),
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
                Ok(mut settings) => {
                    // A name that is exactly what an older version filled in was
                    // never typed by anyone, so it follows the name this version
                    // shows. A name the user chose is not touched.
                    if settings.alias == legacy_alias() {
                        settings.alias = default_alias();
                    }
                    // A phone that called itself a desktop is what every version
                    // before this one defaulted to there. The kind decides one
                    // icon on the peers' lists, so it follows the platform once;
                    // a kind the user chose among the others is left alone.
                    if settings.device_type == DeviceType::Desktop {
                        settings.device_type = device_type();
                    }
                    settings
                }
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
///
/// The user's own name is what identifies a device to whoever is looking at the
/// other end of a transfer: a host name says nothing about who is there. A
/// desktop therefore shows `user@host`, which is also what tells two machines
/// the same person uses apart. Where the platform has no name for its user, what
/// the platform calls the device stands in — a phone has no login, and Android
/// keeps the name its owner gave the phone.
///
/// This is only the default: the name is editable in the settings, and a name
/// someone typed is never replaced.
fn default_alias() -> String {
    platform_device_name()
        .or_else(user_at_host)
        .unwrap_or_else(|| FALLBACK_ALIAS.to_string())
}

/// The name shown when the platform has nothing to offer. Also the name every
/// version before this one fell back to, which is what [`legacy_alias`] looks
/// for in a stored name.
const FALLBACK_ALIAS: &str = "Crabsend";

/// Who is at this machine: the login name, and the machine it is on.
///
/// Both parts are optional — a session without a name, a sandbox without a host
/// name — and one of them alone is still better than nothing.
fn user_at_host() -> Option<String> {
    match (user_name(), hostname()) {
        (Some(user), Some(host)) => Some(format!("{user}@{host}")),
        (Some(user), None) => Some(user),
        (None, host) => host,
    }
}

/// The name the user of this device is known by.
///
/// A desktop keeps it in the password database, where the login name lives; a
/// session also exports it, which is all Windows has, and the fallback for a
/// sandbox that hides the database. A phone's password database is empty — an
/// application's uid has no entry in it — so a phone falls through to the name
/// of the device itself.
fn user_name() -> Option<String> {
    #[cfg(unix)]
    let from_password_database = password_database_name();
    #[cfg(not(unix))]
    let from_password_database: Option<String> = None;

    from_password_database
        .or_else(|| {
            ["USER", "LOGNAME", "USERNAME"]
                .into_iter()
                .find_map(|key| std::env::var(key).ok())
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// The login name the current user id has in the password database.
#[cfg(unix)]
fn password_database_name() -> Option<String> {
    use std::ffi::CStr;
    use std::ffi::c_char;
    use std::ptr;

    // A page is what `_SC_GETPW_R_SIZE_MAX` suggests as room for the largest
    // entry; a name that does not fit in one is not a user's name.
    let mut buffer = vec![0_u8; 1024];
    // SAFETY: the buffer and the entry outlive the call, and only the fields
    // the call filled in are read from `entry`.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = ptr::null_mut();
        let status = libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr() as *mut c_char,
            buffer.len(),
            &mut result,
        );
        if status != 0 || result.is_null() || entry.pw_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(entry.pw_name).to_str().ok()?.trim();
        (!name.is_empty()).then(|| name.to_string())
    }
}

/// What the platform calls this device.
///
/// Android answers this: the activity reads the name the owner gave the phone in
/// the system's own settings and publishes it before this side starts, because
/// only Java can read it. `net.hostname` and `ro.product.model` stand in for a
/// launch that could not ask — the second is what Android itself shows before
/// anyone renames the device.
///
/// Every other platform has a user name instead, so there is nothing to add
/// there.
#[cfg(target_os = "android")]
fn platform_device_name() -> Option<String> {
    std::env::var(DEVICE_NAME_ENV)
        .ok()
        .or_else(|| android_property("net.hostname"))
        .or_else(|| android_property("ro.product.model"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// The variable the activity publishes the device's name through, before it
/// starts the runtime this side runs in.
#[cfg(target_os = "android")]
const DEVICE_NAME_ENV: &str = "CRABSEND_DEVICE_NAME";

/// A desktop has no device name to add: the name it is known by is the user and
/// the host, which [`default_alias`] reads on its own.
#[cfg(not(target_os = "android"))]
fn platform_device_name() -> Option<String> {
    None
}

/// One Android system property, or `None` when it is unset.
#[cfg(target_os = "android")]
fn android_property(name: &str) -> Option<String> {
    use std::ffi::CString;
    use std::ffi::c_char;
    use std::slice;

    // The property store's own limit: a longer value cannot exist.
    const PROP_VALUE_MAX: usize = 92;

    let name = CString::new(name).ok()?;
    let mut value = [0 as c_char; PROP_VALUE_MAX];
    // SAFETY: both pointers are valid for the call, which writes at most
    // `PROP_VALUE_MAX` bytes into `value` and returns how many it wrote.
    let length = unsafe { libc::__system_property_get(name.as_ptr(), value.as_mut_ptr()) };
    if length <= 0 {
        return None;
    }
    // SAFETY: the call reported `length` bytes written, which is what is read.
    let bytes = unsafe { slice::from_raw_parts(value.as_ptr() as *const u8, length as usize) };
    let name = String::from_utf8_lossy(bytes).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The default of every version that showed the host name.
///
/// A stored name equal to this one was never typed by anyone — it is what an
/// older version filled in — so [`Settings::load`] moves it to [`default_alias`].
fn legacy_alias() -> String {
    hostname().unwrap_or_else(|| FALLBACK_ALIAS.to_string())
}

/// The host name, which is what a peer is shown beside the user's name, and what
/// tells the machines of one person apart.
///
/// Asked of the kernel, which every desktop answers. It used to be read out of
/// `/proc/sys/kernel/hostname`, a file only Linux has: a Mac had no host name at
/// all and announced the bare user name, so every Mac of one person looked like
/// the same device to whoever was sending to it. A name no kernel would hand out
/// — empty, or the `localhost` Android reports — is not a name.
fn hostname() -> Option<String> {
    let name = kernel_hostname()?;
    // macOS answers with the machine's Bonjour name, whose `.local` is the
    // service domain rather than part of the name.
    #[cfg(target_os = "macos")]
    let name = name.strip_suffix(".local").unwrap_or(&name).to_string();
    let name = name.trim().to_string();
    if name.is_empty() || name == "localhost" {
        return None;
    }
    Some(name)
}

/// The name the kernel knows this machine by.
#[cfg(unix)]
fn kernel_hostname() -> Option<String> {
    // One byte short of the buffer: a name of the largest length a kernel holds
    // then still leaves the terminator this reads up to.
    let mut buffer = [0 as libc::c_char; 256];
    // SAFETY: the buffer is valid for the call, which writes at most the length
    // it is given and leaves the rest of the buffer untouched.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() - 1) };
    if status != 0 {
        return None;
    }
    // SAFETY: the buffer is zeroed and the call writes at most up to its last
    // byte, so there is a terminator to stop at.
    let name = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

/// Windows has neither `/proc` nor `gethostname`: a build there announces the
/// user name alone, as it did before.
#[cfg(not(unix))]
fn kernel_hostname() -> Option<String> {
    None
}

/// The kind of device this build runs on.
///
/// This is what a peer draws beside the name, so a phone that shows itself as a
/// desktop gets a desktop's icon on every device list it appears in. The user
/// can choose any of the kinds in the settings; this is what an installation
/// starts with, and — because no version before this one had a phone default —
/// what a persisted desktop is moved away from on a phone.
fn device_type() -> DeviceType {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        DeviceType::Mobile
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        DeviceType::Desktop
    }
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
    fn a_name_that_was_never_typed_follows_the_platform_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        // What an older version filled in is not a name anyone chose.
        Settings {
            alias: legacy_alias(),
            ..Settings::default()
        }
        .save(&path)
        .unwrap();
        assert_eq!(Settings::load(&path).alias, default_alias());

        // A name someone typed is left where it is.
        Settings {
            alias: "My Laptop".to_string(),
            ..Settings::default()
        }
        .save(&path)
        .unwrap();
        assert_eq!(Settings::load(&path).alias, "My Laptop");
    }

    /// A Mac used to announce the bare user name: the host name was read out of
    /// a file only Linux has. What a phone lists for this device is this name,
    /// so the machine has to be part of it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_names_the_machine_it_is_on() {
        let name = user_at_host().expect("a user and a host");
        assert!(name.contains('@'), "{name}");
        assert!(!name.ends_with(".local"), "{name}");
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
