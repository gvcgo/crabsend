//! What a peer's address is worth after it registered with us.
//!
//! A peer that announces itself over IPv6 answers from a link-local address,
//! which is only usable together with the interface it belongs to. The address
//! the peer is remembered at therefore has to keep that interface: without it
//! the operating system refuses every connection to the device (`EINVAL`), and
//! a device that answers `Scan` becomes one that cannot be sent to.

use std::net::Ipv6Addr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crabsend_app::settings::Settings;
use crabsend_app::state::AppState;
use crabsend_core::client::HttpClient;
use crabsend_core::client::HttpTarget;
use crabsend_core::crypto::Identity;
use crabsend_core::model::ProtocolType;

/// A device that serves HTTPS on a port of its own.
async fn device(dir: &Path, alias: &str) -> Result<Arc<AppState>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
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

/// The first IPv6 link-local address of this machine and the scope id of the
/// interface it belongs to. `None` where there is none, which is a machine with
/// IPv6 switched off: there is nothing to check there.
///
/// The C list is read rather than `if_addrs`, which leaves link-local addresses
/// out — the very ones this test is about.
fn link_local() -> Option<(Ipv6Addr, u32)> {
    // SAFETY: the list `getifaddrs` fills in is walked while it is alive and
    // freed before returning; every entry is checked for a null `ifa_addr`
    // before it is cast to the family it claims, that family being part of the
    // same union the cast reads.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return None;
        }
        let mut found = None;
        let mut current = head;
        while !current.is_null() && found.is_none() {
            let entry = &*current;
            current = entry.ifa_next;
            if entry.ifa_addr.is_null() || (*entry.ifa_addr).sa_family as i32 != libc::AF_INET6 {
                continue;
            }
            let address = &*(entry.ifa_addr as *const libc::sockaddr_in6);
            let ip = Ipv6Addr::from(address.sin6_addr.s6_addr);
            if ip.is_unicast_link_local() && address.sin6_scope_id != 0 {
                found = Some((ip, address.sin6_scope_id));
            }
        }
        libc::freeifaddrs(head);
        found
    }
}

#[tokio::test]
async fn a_peer_that_registered_over_a_link_local_address_stays_reachable() -> Result<()> {
    let Some((address, scope)) = link_local() else {
        eprintln!("skipping: this machine has no IPv6 link-local address");
        return Ok(());
    };

    let receiver_dir = tempfile::tempdir()?;
    let receiver = device(receiver_dir.path(), "Receiver").await?;
    let receiver_port = receiver.snapshot().server.port;

    let sender_dir = tempfile::tempdir()?;
    let sender = device(sender_dir.path(), "Sender").await?;
    let sender_port = sender.snapshot().server.port;
    let sender_fingerprint = sender.snapshot().device.fingerprint.clone();

    // The sender registers with the receiver over the link-local address, which
    // is what a peer answering `Scan` does.
    sender
        .add_device(&format!("[{address}%{scope}]:{receiver_port}"))
        .await?;

    let offered = receiver
        .snapshot()
        .devices
        .into_iter()
        .find(|device| device.fingerprint == sender_fingerprint)
        .expect("the peer that registered is offered");
    assert_eq!(
        offered.host,
        format!("{address}%{scope}"),
        "the peer is remembered without the interface its address needs"
    );
    assert_eq!(offered.port, sender_port);

    // And that address is one this device can reach: the connection the device
    // list is offered for is the one that used to fail with `EINVAL`.
    let identity = Identity::generate()?;
    let target = HttpTarget::new(ProtocolType::Https, offered.host.clone(), offered.port);
    let client = HttpClient::new(&identity, &target, None, Some(Duration::from_secs(10)))?;
    let answer = client.info().await?;
    assert_eq!(answer.alias, "Sender");

    receiver.stop().await;
    sender.stop().await;
    Ok(())
}
