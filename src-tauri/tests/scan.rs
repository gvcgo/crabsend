//! What a scan does to the device list.
//!
//! A peer that leaves the network, or that comes back under another identity,
//! must not be offered for good: the scan is the moment the list is brought up
//! to date, and whatever answered none of it is dropped.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use crabsend_app::settings::Settings;
use crabsend_app::state::AppState;

/// A device that serves HTTPS on a port of its own.
async fn device(dir: &Path, alias: &str) -> Result<Arc<AppState>> {
    let port = free_port()?;
    Settings {
        alias: alias.to_string(),
        port,
        encryption: true,
        download_dir: dir.join("inbox"),
        ..Settings::default()
    }
    .save(&dir.join("settings.json"))?;

    let state = Arc::new(AppState::new_headless(dir.to_path_buf())?);
    state.restart().await;
    anyhow::ensure!(
        state.snapshot().server.running,
        "the test device did not start its server"
    );
    Ok(state)
}

/// Asks the operating system for a port nobody is listening on.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// The code a device serves, dialled on loopback: a test cannot rely on the
/// machine owning a LAN address the other instance reaches.
async fn loopback_code(state: &AppState) -> Result<String> {
    use crabsend_app::pairing::PairingPayload;

    let code = state.pairing_qr().await?;
    let mut payload = PairingPayload::parse(&code.payload)?;
    payload.addresses = vec!["127.0.0.1".to_string()];
    payload.to_json()
}

#[tokio::test]
async fn a_paired_device_that_comes_back_under_another_identity_is_offered_once() -> Result<()> {
    let peer_dir = tempfile::tempdir()?;
    let peer = device(peer_dir.path(), "Peer").await?;
    let port = peer.snapshot().server.port;

    let own_dir = tempfile::tempdir()?;
    let own = device(own_dir.path(), "Own").await?;
    own.pair_from_qr(&loopback_code(&peer).await?).await?;
    assert_eq!(
        own.snapshot().devices.len(),
        1,
        "the paired device is offered"
    );

    // The peer comes back with a certificate of its own making: another
    // fingerprint at the same address, which is the same device still.
    peer.stop().await;
    std::fs::remove_file(peer_dir.path().join("identity.pem"))?;
    let peer = Arc::new(AppState::new_headless(peer_dir.path().to_path_buf())?);
    peer.restart().await;
    anyhow::ensure!(peer.snapshot().server.port == port, "the peer moved port");

    own.add_device(&format!("127.0.0.1:{port}")).await?;
    let devices = own.snapshot().devices;
    let offered: Vec<String> = devices
        .iter()
        .map(|device| format!("{}@{}:{}", device.alias, device.host, device.port))
        .collect();
    assert_eq!(
        devices.len(),
        1,
        "the same device is offered once: {offered:?}"
    );
    assert_eq!(
        devices[0].fingerprint,
        peer.snapshot().device.fingerprint,
        "the sighting at that address is the one offered"
    );

    own.stop().await;
    peer.stop().await;
    Ok(())
}

/// Waits for the scan, which runs in the background and reports by state.
async fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test]
async fn a_scan_drops_a_device_that_stopped_answering() -> Result<()> {
    let peer_dir = tempfile::tempdir()?;
    let peer = device(peer_dir.path(), "Peer").await?;
    let peer_fingerprint = peer.snapshot().device.fingerprint.clone();

    let own_dir = tempfile::tempdir()?;
    let own = device(own_dir.path(), "Own").await?;

    // A user types the peer's address in, and it answers.
    own.add_device(&format!("127.0.0.1:{}", peer.snapshot().server.port))
        .await?;
    assert!(
        own.snapshot()
            .devices
            .iter()
            .any(|device| device.fingerprint == peer_fingerprint),
        "the device that answered is offered"
    );

    // The peer leaves; nothing answers for it any more.
    peer.stop().await;
    own.scan().await;
    assert!(
        wait_for(|| {
            !own.snapshot()
                .devices
                .iter()
                .any(|device| device.fingerprint == peer_fingerprint)
        })
        .await,
        "the scan did not drop the device that stopped answering"
    );

    own.stop().await;
    Ok(())
}
