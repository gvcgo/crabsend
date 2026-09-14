//! Inspecting the files a user picked.

use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use percent_encoding::percent_decode_str;
use tauri::AppHandle;
use tauri::Manager;
use tauri_plugin_fs::FilePath;
use tauri_plugin_fs::FsExt;
use tauri_plugin_fs::OpenOptions;

use crate::state::SendFile;

/// How many entries one selection may produce; a folder drop of a whole home
/// directory must not lock the UI up.
const MAX_ENTRIES: usize = 10_000;

/// How deep folder transfers are followed.
const MAX_DEPTH: usize = 32;

/// What Android's document picker returns instead of a path.
const CONTENT_URI_PREFIX: &str = "content://";

/// Turns what the picker returned into paths this application can open.
///
/// Android hands out `content://` URIs, which are not files but handles to the
/// provider that issued them: only that provider can read the bytes. Such a
/// file is read once and copied into the application's cache, where hashing and
/// uploading open it like any other file, so the rest of the application needs
/// to know nothing about it.
pub fn ingest(app: &AppHandle, paths: &[String]) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(paths.len());
    for path in paths {
        if path.starts_with(CONTENT_URI_PREFIX) {
            resolved.push(materialize(app, path)?);
        } else {
            resolved.push(path.clone());
        }
    }
    Ok(resolved)
}

/// Copies one `content://` URI into the cache and returns the copy's path.
fn materialize(app: &AppHandle, uri: &str) -> Result<String> {
    // The digest of the URI keeps two files of the same name apart, and giving
    // each its own directory keeps it out of the transfer's name.
    let digest = crabsend_core::crypto::sha256_hex_bytes(uri.as_bytes());
    let directory = app
        .path()
        .app_cache_dir()
        .context("locating the cache directory")?
        .join("picked")
        .join(&digest[..8]);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let target = directory.join(file_name_of(uri));

    let mut options = OpenOptions::new();
    options.read(true);
    let mut source = app
        .fs()
        .open(content_uri(uri)?, options)
        .with_context(|| format!("opening {uri}"))?;
    let mut copy =
        std::fs::File::create(&target).with_context(|| format!("writing {}", target.display()))?;
    std::io::copy(&mut source, &mut copy).with_context(|| format!("copying {uri}"))?;
    tracing::info!("read {uri} into {}", target.display());
    Ok(target.display().to_string())
}

/// A `content://` URI in the form the file-system plugin resolves: through the
/// provider that issued it, down to a real file descriptor.
fn content_uri(uri: &str) -> Result<FilePath> {
    let url = url::Url::parse(uri).with_context(|| format!("{uri} is not a URI"))?;
    Ok(FilePath::from(url))
}

/// The name the provider gave the file, which is what the transfer will carry.
///
/// A `content://` URI ends in something readable for most pickers — a plain file
/// name for the file explorers, or a document id such as
/// `primary:DCIM/Screenshots/Shot.jpg` for Android's own — but the id is percent
/// encoded and can also be an opaque number, in which case there is no name to
/// be had and a placeholder is the honest answer.
fn file_name_of(uri: &str) -> String {
    let decoded = percent_decode_str(uri).decode_utf8_lossy();
    let segment = decoded.rsplit('/').next().unwrap_or_default();
    // A document id carries its real path after the colon.
    let candidate = segment.rsplit(':').next().unwrap_or_default();
    let opaque = !candidate.contains('.') && candidate.chars().all(|c| c.is_ascii_digit());
    if candidate.is_empty() || opaque || candidate == "." || candidate == ".." {
        "picked-file".to_string()
    } else {
        candidate.to_string()
    }
}

/// Describes the picked paths, without reading file contents.
///
/// A folder is offered with its own name as the leading path component, which
/// is what the receiver uses to recreate the structure.
pub fn inspect(paths: &[String]) -> Result<Vec<SendFile>> {
    let mut files = Vec::new();
    for path in paths {
        let path = PathBuf::from(path);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if metadata.is_dir() {
            collect_directory(&path, &mut files)?;
        } else if metadata.is_file() {
            let name = file_name(&path);
            files.push(describe(&path, name, &metadata));
        }
    }
    Ok(files)
}

fn collect_directory(directory: &Path, out: &mut Vec<SendFile>) -> Result<()> {
    let root_name = file_name(directory);
    let mut stack = vec![(directory.to_path_buf(), root_name, 0usize)];
    while let Some((path, prefix, depth)) = stack.pop() {
        if depth > MAX_DEPTH || out.len() >= MAX_ENTRIES {
            continue;
        }
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!("skipping {}: {error}", path.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_ENTRIES {
                tracing::warn!("stopping at {MAX_ENTRIES} files");
                return Ok(());
            }
            let entry_path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let name = format!("{prefix}/{}", file_name(&entry_path));
            if metadata.is_dir() {
                stack.push((entry_path, name, depth + 1));
            } else if metadata.is_file() {
                out.push(describe(&entry_path, name, &metadata));
            }
        }
    }
    Ok(())
}

fn describe(path: &Path, name: String, metadata: &std::fs::Metadata) -> SendFile {
    SendFile {
        path: path.display().to_string(),
        mime: mime_guess::from_path(path)
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_string(),
        name,
        size: metadata.len(),
        sha256: None,
        modified: metadata.modified().ok().and_then(format_timestamp),
        accessed: metadata.accessed().ok().and_then(format_timestamp),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "untitled".to_string())
}

/// Renders a timestamp the way the protocol expects it: RFC 3339.
fn format_timestamp(time: SystemTime) -> Option<String> {
    let time = time.duration_since(UNIX_EPOCH).ok()?;
    let offset = time::OffsetDateTime::from_unix_timestamp(time.as_secs() as i64).ok()?;
    offset
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_described_with_its_mime_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        std::fs::write(&path, b"hello").unwrap();
        let files = inspect(&[path.display().to_string()]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "note.txt");
        assert_eq!(files[0].size, 5);
        assert_eq!(files[0].mime, "text/plain");
        assert!(files[0].modified.is_some());
    }

    #[test]
    fn a_folder_keeps_its_name_as_the_leading_component() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("photos");
        std::fs::create_dir_all(folder.join("2024")).unwrap();
        std::fs::write(folder.join("2024/img.png"), b"x").unwrap();
        std::fs::write(folder.join("top.txt"), b"y").unwrap();

        let mut files = inspect(&[folder.display().to_string()]).unwrap();
        files.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(names, ["photos/2024/img.png", "photos/top.txt"]);
        assert_eq!(files[0].mime, "image/png");
    }

    #[test]
    fn unknown_extensions_fall_back_to_octet_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.unknownext");
        std::fs::write(&path, b"x").unwrap();
        let files = inspect(&[path.display().to_string()]).unwrap();
        assert_eq!(files[0].mime, "application/octet-stream");
    }

    #[test]
    fn a_missing_path_is_an_error() {
        assert!(inspect(&["/definitely/not/here".to_string()]).is_err());
    }

    #[test]
    fn a_content_uri_yields_the_name_its_provider_gave_the_file() {
        // A file explorer hands out the path it is showing.
        assert_eq!(
            file_name_of(
                "content://com.android.fileexplorer.myprovider/external_files/DCIM/Screenshots/Shot.jpg"
            ),
            "Shot.jpg"
        );
        // Android's own picker uses a document id, which carries the real path.
        assert_eq!(
            file_name_of(
                "content://com.android.externalstorage.documents/document/primary%3ADCIM%2FScreenshots%2FShot.jpg"
            ),
            "Shot.jpg"
        );
        assert_eq!(
            file_name_of("content://provider/files/My%20shot.jpg"),
            "My shot.jpg"
        );
        // An opaque id has nothing to name the copy after.
        assert_eq!(
            file_name_of("content://com.android.providers.media.documents/document/document%3A40"),
            "picked-file"
        );
        assert_eq!(file_name_of("content://"), "picked-file");
    }
}
