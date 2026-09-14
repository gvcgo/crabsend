//! Interop check against the official LocalSend implementation.
//!
//! The counterpart is the official `localsend-cli` (LocalSend's own Rust
//! client and server), driven headlessly: it auto-accepts transfers from
//! fingerprints listed in its `paired-v2.json`. Point the environment variable
//! `LOCALSEND_CLI` at that binary to run this test; without it the test is
//! skipped, because no other implementation is available offline.
//!
//! ```text
//! LOCALSEND_CLI=/path/to/localsend-cli cargo test -p crabsend-app --test official_peer
//! ```

use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use crabsend_app::settings::Settings;
use crabsend_app::state::AppState;
use crabsend_app::state::SendFile;
use crabsend_app::state::SessionStatus;
use serde_json::json;

/// The official CLI always announces and probes on the protocol default.
const CLI_PORT: u16 = 53317;

#[tokio::test]
async fn the_app_sends_a_file_to_the_official_cli() -> Result<()> {
    let Some(binary) = std::env::var_os("LOCALSEND_CLI") else {
        eprintln!("skipping: set LOCALSEND_CLI to the official localsend-cli binary");
        return Ok(());
    };
    let binary = PathBuf::from(binary);
    anyhow::ensure!(binary.is_file(), "{} is not a file", binary.display());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUST_LOG").unwrap_or_default(),
        )
        .try_init();

    // Our application, headless, on a port of its own.
    let app_dir = tempfile::tempdir()?;
    let state = Arc::new(AppState::new_headless(app_dir.path().to_path_buf())?);
    let settings = Settings {
        port: free_port()?,
        encryption: true,
        download_dir: app_dir.path().join("inbox"),
        ..Settings::default()
    };
    state.update_settings(settings);
    state.restart().await;
    let snapshot = state.snapshot();
    anyhow::ensure!(
        snapshot.server.running,
        "the app's server did not start: {:?}",
        snapshot.server.error
    );

    // The official peer, paired with this device so it accepts without a prompt.
    let cli_config = app_dir.path().join("official");
    let cli_dir = cli_config.join("localsend-cli");
    std::fs::create_dir_all(&cli_dir)?;
    std::fs::write(
        cli_dir.join("paired-v2.json"),
        serde_json::to_string(&json!({
            "version": 1,
            "devices": {
                state.identity.fingerprint.clone(): {
                    "alias": "crabsend-test",
                    "channels": [{ "host": "127.0.0.1", "port": snapshot.server.port, "protocol": "HTTPS" }],
                }
            }
        }))?,
    )?;
    let destination = app_dir.path().join("official-inbox");
    std::fs::create_dir_all(&destination)?;
    let mut cli = spawn_cli(&binary, &cli_config, &destination, app_dir.path())?;

    let result = run_transfer(&state, &destination, app_dir.path()).await;
    let _ = cli.kill();
    let _ = cli.wait();
    state.stop().await;
    if let Err(error) = &result {
        // The peer's terminal output explains why it did not answer.
        let log = std::fs::read(app_dir.path().join("official-peer.log")).unwrap_or_default();
        let log = String::from_utf8_lossy(&log);
        eprintln!("official peer log:\n{}", strip_escape_codes(&log));
        return Err(anyhow::anyhow!("{error:#}"));
    }
    result
}

/// Removes the terminal escape sequences of the peer's output.
fn strip_escape_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// Sends one file to the peer and checks that it arrived byte for byte.
async fn run_transfer(
    state: &Arc<AppState>,
    destination: &PathBuf,
    work: &std::path::Path,
) -> Result<()> {
    wait_for_peer(state).await?;

    let device = state
        .snapshot()
        .devices
        .first()
        .cloned()
        .context("the app did not discover the official peer")?;
    anyhow::ensure!(
        device.alias.contains("Crabsend-Official") || !device.alias.is_empty(),
        "unexpected peer {}",
        device.alias
    );

    let source = work.join("payload.bin");
    let payload: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &payload)?;
    let file = SendFile {
        path: source.display().to_string(),
        name: "payload.bin".to_string(),
        size: payload.len() as u64,
        mime: "application/octet-stream".to_string(),
        sha256: None,
        modified: None,
        accessed: None,
    };

    let session_id = state.start_send(&device.fingerprint, vec![file], None)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snapshot = state.snapshot();
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .context("the send session disappeared")?;
        if session.status.is_terminal() {
            anyhow::ensure!(
                session.status == SessionStatus::Done,
                "the official peer refused the transfer: {:?} {:?}",
                session.status,
                session.error
            );
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the transfer did not finish in time: {:?}",
            session.status
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let received = destination.join("payload.bin");
    let bytes = std::fs::read(&received)
        .with_context(|| format!("the official peer did not store {}", received.display()))?;
    anyhow::ensure!(
        bytes == payload,
        "the received file differs from the source"
    );
    Ok(())
}

/// Registers with the official peer, which also proves our TLS client and our
/// client certificate are accepted by it.
async fn wait_for_peer(state: &Arc<AppState>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the official peer never answered"
        );
        state.add_device("127.0.0.1").await.ok();
        if !state.snapshot().devices.is_empty() {
            return Ok(());
        }
        // The peer may still be starting up; probe again shortly.
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !state.snapshot().devices.is_empty() {
            return Ok(());
        }
    }
}

/// Starts the official CLI as a headless receiver.
///
/// Its receiver is a terminal UI, so it runs under a pseudo terminal.
fn spawn_cli(
    binary: &PathBuf,
    config_home: &PathBuf,
    destination: &PathBuf,
    work: &std::path::Path,
) -> Result<Child> {
    std::fs::create_dir_all(destination)?;
    // The receiver is a terminal UI; its output goes to a file so a full pipe
    // cannot stall it.
    let output = std::fs::File::create(work.join("official-peer.log"))?;
    let command = format!(
        "exec {} --alias Crabsend-Official --port {CLI_PORT} --destination {}",
        shell_quote(&binary.display().to_string()),
        shell_quote(&destination.display().to_string())
    );
    Command::new("script")
        .arg("-qec")
        .arg(command)
        .arg("/dev/null")
        .env("XDG_CONFIG_HOME", config_home)
        .env("TERM", "xterm-256color")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(output.try_clone()?))
        .stderr(std::process::Stdio::from(output))
        .spawn()
        .context("starting the official CLI under a pseudo terminal")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// A port the OS reports as free.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}
