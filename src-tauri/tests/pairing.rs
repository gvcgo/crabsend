//! Pairing through a QR code, against a real HTTPS peer.
//!
//! The code is what makes a scan trustworthy: the fingerprint inside it is
//! pinned to the peer's certificate, so a device that answers in its place
//! never receives a request, let alone a file. These tests pair two headless
//! instances over loopback and use the payload each device really produces.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use crabsend_app::files;
use crabsend_app::pairing::PairingPayload;
use crabsend_app::settings::Settings;
use crabsend_app::state::AppState;
use crabsend_core::crypto::Identity;

/// Serializes "pick a port, bind it".
///
/// These tests run at once, and a port that `free_port` reports as free can be
/// taken by a sibling before the server reaches it.
static STARTUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A device that serves HTTPS on a port of its own.
async fn device(dir: &Path, alias: &str) -> Result<Arc<AppState>> {
    // The settings are written down before the state exists: `update_settings`
    // restarts the server itself, so pairing it with an explicit restart here
    // would have two starts racing for the same port.
    let _starting = STARTUP.lock().await;
    Settings {
        alias: alias.to_string(),
        port: free_port()?,
        encryption: true,
        download_dir: dir.join("inbox"),
        ..Settings::default()
    }
    .save(&dir.join("settings.json"))?;

    let state = Arc::new(AppState::new_headless(dir.to_path_buf())?);
    state.restart().await;
    let snapshot = state.snapshot();
    anyhow::ensure!(
        snapshot.server.running,
        "the test device did not start its server: {:?}",
        snapshot.server.error
    );
    Ok(state)
}

/// The code a device serves, dialled on loopback.
///
/// The payload is the one the device itself produced — a test cannot rely on
/// the machine owning a LAN address the other instance can reach, so only the
/// addresses are replaced.
async fn loopback_code(state: &AppState) -> Result<String> {
    let code = state.pairing_qr().await?;
    let mut payload = PairingPayload::parse(&code.payload)?;
    let snapshot = state.snapshot();
    assert_eq!(payload.fingerprint, snapshot.device.fingerprint);
    assert_eq!(payload.port, snapshot.server.port);
    assert_eq!(payload.protocol, snapshot.server.protocol);
    assert_eq!(payload.alias, snapshot.settings.alias);
    assert!(code.svg.contains("<svg"), "the code renders as SVG");
    payload.addresses = vec!["127.0.0.1".to_string()];
    payload.to_json()
}

#[tokio::test]
async fn a_scanned_code_pairs_with_the_device_that_showed_it() -> Result<()> {
    let owner_dir = tempfile::tempdir()?;
    let owner = device(owner_dir.path(), "Code owner").await?;

    let scanner_dir = tempfile::tempdir()?;
    let scanner = Arc::new(AppState::new_headless(scanner_dir.path().to_path_buf())?);

    let paired = scanner.pair_from_qr(&loopback_code(&owner).await?).await?;
    assert_eq!(paired.fingerprint, owner.snapshot().device.fingerprint);
    assert!(paired.paired);
    // The name and the model come from the peer's own answer to the register.
    assert_eq!(paired.alias, "Code owner");
    assert_eq!(paired.host, "127.0.0.1");
    assert_eq!(paired.port, owner.snapshot().server.port);

    // The peer is offered as a device…
    let listed = scanner.snapshot().devices;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].paired);

    // …and can be sent to, which is what pairing is for.
    let file = scanner_dir.path().join("note.txt");
    std::fs::write(&file, b"hello")?;
    let session = scanner.start_send(
        &paired.fingerprint,
        files::inspect(&[file.display().to_string()])?,
        None,
    )?;
    assert_eq!(
        scanner.snapshot().sessions[0].peer.fingerprint,
        paired.fingerprint
    );
    scanner.cancel_session(&session).await;

    // A pairing outlives the process that made it.
    let reloaded = Arc::new(AppState::new_headless(scanner_dir.path().to_path_buf())?);
    let listed = reloaded.snapshot().devices;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].fingerprint, paired.fingerprint);
    assert_eq!(listed[0].alias, "Code owner");

    // Forgetting one removes it on disk as well.
    reloaded.unpair(&paired.fingerprint)?;
    assert!(reloaded.snapshot().devices.is_empty());
    let forgotten = Arc::new(AppState::new_headless(scanner_dir.path().to_path_buf())?);
    assert!(forgotten.snapshot().devices.is_empty());

    owner.stop().await;
    Ok(())
}

#[tokio::test]
async fn a_code_naming_another_identity_never_reaches_the_device() -> Result<()> {
    let owner_dir = tempfile::tempdir()?;
    let owner = device(owner_dir.path(), "Real device").await?;

    // The address, the port and the name are the real ones; only the
    // fingerprint belongs to a device that is not the one answering.
    let mut forged = PairingPayload::parse(&loopback_code(&owner).await?)?;
    forged.fingerprint = Identity::generate()?.fingerprint;

    let scanner_dir = tempfile::tempdir()?;
    let scanner = Arc::new(AppState::new_headless(scanner_dir.path().to_path_buf())?);
    assert!(scanner.pair_from_qr(&forged.to_json()?).await.is_err());
    assert!(scanner.snapshot().devices.is_empty());
    assert!(
        owner.snapshot().devices.is_empty(),
        "the pin must fail the handshake, so the peer is not even registered"
    );

    owner.stop().await;
    Ok(())
}

#[tokio::test]
async fn this_devices_own_code_is_refused() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let state = device(dir.path(), "Myself").await?;
    assert!(
        state
            .pair_from_qr(&loopback_code(&state).await?)
            .await
            .is_err()
    );
    assert!(state.snapshot().devices.is_empty());
    state.stop().await;
    Ok(())
}

#[tokio::test]
async fn starting_a_send_needs_no_runtime_context() -> Result<()> {
    // Tauri runs synchronous commands on the main thread, which has no runtime
    // context. Starting a transfer there must not depend on one: a bare
    // `tokio::spawn` used to panic, and that panic could not unwind out of the
    // webview's callback, so the whole application aborted instead.
    let owner_dir = tempfile::tempdir()?;
    let owner = device(owner_dir.path(), "Code owner").await?;
    let scanner_dir = tempfile::tempdir()?;
    let scanner = Arc::new(AppState::new_headless(scanner_dir.path().to_path_buf())?);
    let paired = scanner.pair_from_qr(&loopback_code(&owner).await?).await?;

    let file = scanner_dir.path().join("note.txt");
    std::fs::write(&file, b"hello")?;
    let files = files::inspect(&[file.display().to_string()])?;

    let starter = scanner.clone();
    let target = paired.fingerprint.clone();
    let session = std::thread::spawn(move || starter.start_send(&target, files, None))
        .join()
        .expect("starting a send must not panic outside a runtime")?;
    assert_eq!(
        scanner.snapshot().sessions[0].peer.fingerprint,
        paired.fingerprint
    );
    scanner.cancel_session(&session).await;

    owner.stop().await;
    Ok(())
}

/// A port the OS reports as free.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}
