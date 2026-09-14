//! Destination-path policy for received files.
//!
//! The protocol lets a sender name a file with relative path components
//! (folder transfers), so every component is sanitized and the final path is
//! proven to stay inside the download directory. Getting this wrong means a
//! malicious peer can write anywhere on disk.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

/// Longest file name component kept, in bytes (ext4/APFS/NTFS all allow 255).
const MAX_COMPONENT_BYTES: usize = 255;

/// Characters that are invalid on Windows and/or POSIX filesystems.
const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Device names Windows refuses as file names, with or without an extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Sanitizes a single path component.
///
/// Control characters and characters forbidden on any supported platform are
/// replaced by `_`; leading dots, trailing dots/spaces and reserved device
/// names are defused. The result is never empty, `.` or `..`.
pub fn sanitize_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || FORBIDDEN.contains(&ch) {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    // Trailing dots and spaces are silently dropped by Windows, which makes
    // distinct names collide there.
    let trimmed = out.trim_end_matches(['.', ' ']).to_string();
    let mut out = trimmed;

    // A leading dot hides the file on POSIX and, for `.`/`..`, would traverse.
    if out.starts_with('.') {
        out.replace_range(0..1, "_");
    }
    if out.is_empty() {
        out.push_str("untitled");
    }
    if RESERVED
        .iter()
        .any(|reserved| out.eq_ignore_ascii_case(reserved))
    {
        out.insert(0, '_');
    }
    truncate_bytes(&mut out, MAX_COMPONENT_BYTES);
    out
}

/// Splits a sender-provided file name into sanitized relative components.
///
/// Absolute paths and `..` components are dropped rather than followed, so the
/// result always describes a location *below* the download directory.
pub fn sanitize_relative_path(file_name: &str) -> Vec<String> {
    let normalized = file_name.replace('\\', "/");
    let mut components = Vec::new();
    for raw in normalized.split('/') {
        let raw = raw.trim();
        if raw.is_empty() || raw == "." || raw == ".." {
            continue;
        }
        let component = sanitize_component(raw);
        if component.is_empty() {
            continue;
        }
        components.push(component);
    }
    if components.is_empty() {
        components.push("untitled".to_string());
    }
    components
}

/// Builds the destination path of a received file inside `dir`, creating no
/// directories and picking a free name on collision.
///
/// Returns the path and whether a ` (n)` suffix had to be added.
pub fn resolve_destination(dir: &Path, file_name: &str) -> Result<(PathBuf, bool)> {
    let components = sanitize_relative_path(file_name);
    let mut path = dir.to_path_buf();
    for component in &components[..components.len() - 1] {
        path.push(component);
    }
    let file = components
        .last()
        .expect("sanitize_relative_path always yields a component")
        .clone();
    path.push(file);

    // Defense in depth: nothing above the download directory may be addressed.
    let relative = path
        .strip_prefix(dir)
        .with_context(|| format!("refusing to write outside {}", dir.display()))?;
    anyhow::ensure!(
        !relative
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "refusing to write outside {}",
        dir.display()
    );

    let (unique, renamed) = unique_path(&path);
    Ok((unique, renamed))
}

/// Returns `path`, or the first `name (n).ext` variant that does not exist yet.
fn unique_path(path: &Path) -> (PathBuf, bool) {
    if !path.exists() {
        return (path.to_path_buf(), false);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "untitled".to_string());
    let extension = path.extension().map(|e| e.to_string_lossy().to_string());
    for n in 1..10_000u32 {
        let candidate = match &extension {
            Some(extension) => parent.join(format!("{stem} ({n}).{extension}")),
            None => parent.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return (candidate, true);
        }
    }
    (path.to_path_buf(), false)
}

fn truncate_bytes(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_traversal_and_absolute_components() {
        assert_eq!(
            sanitize_relative_path("../../etc/passwd"),
            vec!["etc", "passwd"]
        );
        assert_eq!(sanitize_relative_path("/etc/passwd"), vec!["etc", "passwd"]);
        assert_eq!(sanitize_relative_path(".."), vec!["untitled"]);
    }

    #[test]
    fn keeps_relative_folder_structure() {
        assert_eq!(
            sanitize_relative_path("photos/2024/img.png"),
            vec!["photos", "2024", "img.png"]
        );
    }

    #[test]
    fn replaces_platform_specific_characters() {
        assert_eq!(sanitize_component("a:b*c?d"), "a_b_c_d");
        assert_eq!(sanitize_component("trailing. "), "trailing");
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component(".hidden"), "_hidden");
        assert_eq!(sanitize_component("\u{7}f"), "_f");
    }

    #[test]
    fn truncates_long_names_on_a_char_boundary() {
        let long = "é".repeat(400);
        let sanitized = sanitize_component(&long);
        assert!(sanitized.len() <= MAX_COMPONENT_BYTES);
        assert!(sanitized.chars().all(|c| c == 'é'));
    }

    #[test]
    fn resolves_into_the_download_directory_without_leaving_it() {
        let dir = tempfile::tempdir().unwrap();
        let (path, renamed) = resolve_destination(dir.path(), "sub/../../escape.txt").unwrap();
        assert!(!renamed);
        assert!(path.starts_with(dir.path()));
        assert_eq!(
            path.strip_prefix(dir.path()).unwrap(),
            Path::new("sub/escape.txt")
        );
    }

    #[test]
    fn picks_a_free_name_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.pdf"), b"x").unwrap();
        let (first, renamed) = resolve_destination(dir.path(), "report.pdf").unwrap();
        assert!(renamed);
        assert_eq!(first.file_name().unwrap(), "report (1).pdf");
        std::fs::write(&first, b"y").unwrap();
        let (second, _) = resolve_destination(dir.path(), "report.pdf").unwrap();
        assert_eq!(second.file_name().unwrap(), "report (2).pdf");
    }
}
