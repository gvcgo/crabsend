//! Peer discovery: multicast announcements, HTTP probes and the subnet scan.
//!
//! Sockets are bound per network interface because a multicast socket only
//! sends on one interface, and announcements must leave through all of them.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use futures_util::StreamExt;
use futures_util::stream;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client::DISCOVERY_TIMEOUT;
use crate::client::HttpClient;
use crate::client::HttpTarget;
use crate::crypto::Identity;
use crate::model::DEFAULT_MULTICAST_GROUP;
use crate::model::DEFAULT_MULTICAST_GROUP_V6;
use crate::model::DeviceInfoDto;
use crate::model::DeviceType;
use crate::model::MulticastMessage;
use crate::model::PROTOCOL_VERSION;
use crate::model::ProtocolType;
use crate::model::RegisterDto;

/// Delay before each datagram of an announcement burst.
///
/// A single datagram is easily lost and a device that just joined the network
/// may not be listening yet, so an announcement is repeated.
const ANNOUNCE_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_millis(2000),
];

/// How many hosts a subnet scan probes at once.
const SCAN_CONCURRENCY: usize = 50;

/// Largest datagram accepted; anything bigger is truncated and unparsable.
const RECEIVE_BUFFER_SIZE: usize = 65_536;

/// A peer that answered, as the application sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredDevice {
    pub fingerprint: String,
    pub alias: String,
    pub version: String,
    pub device_model: Option<String>,
    pub device_type: Option<DeviceType>,
    pub download: bool,
    pub protocol: ProtocolType,
    pub host: String,
    pub port: u16,
    pub last_seen: SystemTime,
}

/// Discovery notifications.
#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    /// A peer was seen for the first time in this run.
    Found(DiscoveredDevice),
    /// A known peer answered again.
    Updated(DiscoveredDevice),
    /// Every multicast socket is gone; discovery keeps working over HTTP only.
    MulticastFailed { error: String },
}

/// How to announce and how to reach back.
pub struct DiscoveryConfig {
    /// The port our own HTTP server listens on.
    pub port: u16,
    /// The transport our own HTTP server serves.
    pub protocol: ProtocolType,
    /// What to announce about this device.
    pub alias: String,
    pub device_model: Option<String>,
    pub device_type: Option<DeviceType>,
    pub fingerprint: String,
    /// Whether our browser download API is active.
    pub download: bool,
    /// Presents this device's certificate when talking to peers over HTTPS.
    pub identity: Identity,
}

impl DiscoveryConfig {
    fn announcement(&self) -> MulticastMessage {
        MulticastMessage {
            alias: self.alias.clone(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: self.device_model.clone(),
            device_type: self.device_type,
            fingerprint: self.fingerprint.clone(),
            port: self.port,
            protocol: self.protocol,
            download: self.download,
            announce: true,
        }
    }

    fn register_dto(&self) -> RegisterDto {
        RegisterDto {
            alias: self.alias.clone(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: self.device_model.clone(),
            device_type: self.device_type,
            fingerprint: self.fingerprint.clone(),
            port: self.port,
            protocol: self.protocol,
            download: self.download,
        }
    }
}

/// A running discovery service.
pub struct Discovery {
    state: Arc<State>,
    shutdown: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct State {
    config: DiscoveryConfig,
    devices: Mutex<Vec<DiscoveredDevice>>,
    /// The sockets an announcement is sent through, one per interface.
    sockets: Mutex<Sockets>,
    events: mpsc::UnboundedSender<DiscoveryEvent>,
}

impl Discovery {
    /// Binds the multicast sockets and starts receiving.
    ///
    /// Failing to bind every socket is not fatal: HTTP discovery still works.
    pub async fn start(
        config: DiscoveryConfig,
        events: mpsc::UnboundedSender<DiscoveryEvent>,
    ) -> Result<Self> {
        let state = Arc::new(State {
            config,
            devices: Mutex::new(Vec::new()),
            sockets: Mutex::new(Vec::new()),
            events,
        });
        let shutdown = CancellationToken::new();
        let mut tasks = Vec::new();

        let sockets = bind_multicast_sockets(state.config.port);
        if sockets.is_empty() {
            tracing::warn!("no usable multicast interface; discovery is HTTP-only");
            let _ = state.events.send(DiscoveryEvent::MulticastFailed {
                error: "no usable multicast interface".to_string(),
            });
        }
        for (socket, target) in &sockets {
            tasks.push(tokio::spawn(receive_loop(
                state.clone(),
                socket.clone(),
                *target,
                shutdown.clone(),
            )));
        }
        *state.sockets.lock() = sockets;

        Ok(Self {
            state,
            shutdown,
            tasks,
        })
    }

    /// Sends the announcement burst on every interface.
    pub async fn announce(&self) {
        let message = serde_json::to_vec(&self.state.config.announcement())
            .expect("the announcement always serializes");
        // Copied out of the lock: the guard must not be held across an await.
        let sockets: Sockets = self.state.sockets.lock().clone();
        for delay in ANNOUNCE_DELAYS {
            tokio::time::sleep(delay).await;
            for (socket, target) in sockets.iter() {
                // A failing interface must not stop the others.
                if let Err(error) = socket.send_to(&message, target).await {
                    tracing::debug!("announcement via {target} failed: {error}");
                }
            }
        }
    }

    /// The peers seen so far.
    pub fn devices(&self) -> Vec<DiscoveredDevice> {
        self.state.devices.lock().clone()
    }

    /// Records a peer the application learned about somewhere else, e.g. one
    /// that registered with our own server.
    pub fn add(&self, device: DiscoveredDevice) {
        self.state.store(device);
    }

    /// Forgets every discovered peer.
    pub fn clear(&self) {
        self.state.devices.lock().clear();
    }

    /// Forgets every peer that has not been seen since `since`, returning how
    /// many entries were dropped.
    ///
    /// A scan is what keeps this list honest: a peer answers it by announcing
    /// itself or by answering a probe, and one that answers neither is gone —
    /// which is how a device that left the network, or that came back under
    /// another identity, stops being offered.
    pub fn forget_unseen(&self, since: SystemTime) -> usize {
        forget_unseen(&mut self.state.devices.lock(), since)
    }

    /// Registers with one host, returning the peer when it answers.
    ///
    /// HTTPS is tried first; a peer that only serves HTTP is found as well.
    pub async fn probe_host(&self, host: &str, port: u16) -> Option<DiscoveredDevice> {
        for protocol in [ProtocolType::Https, ProtocolType::Http] {
            if let Some(device) = self
                .state
                .probe(&strip_brackets(host), port, protocol, None)
                .await
            {
                return Some(device);
            }
        }
        None
    }

    /// Probes every host of the `/24` around each local address.
    pub async fn scan_subnet(&self, local_addresses: &[Ipv4Addr]) -> Vec<DiscoveredDevice> {
        let port = self.state.config.port;
        let protocol = self.state.config.protocol;
        let candidates: Vec<Ipv4Addr> = local_addresses
            .iter()
            .flat_map(|address| {
                let octets = address.octets();
                (0..=255u8)
                    .map(move |host| Ipv4Addr::new(octets[0], octets[1], octets[2], host))
                    .filter(move |ip| ip != address)
            })
            .collect();

        let found = stream::iter(candidates.into_iter().map(|ip| {
            let state = self.state.clone();
            async move { state.probe(&ip.to_string(), port, protocol, None).await }
        }))
        .buffer_unordered(SCAN_CONCURRENCY)
        .filter_map(|device| async move { device })
        .collect::<Vec<_>>()
        .await;
        found
    }

    /// Stops receiving announcements.
    pub async fn stop(&self) {
        self.shutdown.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl State {
    /// Merges a confirmation into the device list and notifies the application.
    ///
    /// This device itself is never a peer, however it was learned about: a
    /// phone whose interfaces share a subnet probes its own address, and the
    /// registration that reaches its own server then looks like a peer nothing
    /// else distinguishes from a real one.
    fn store(&self, device: DiscoveredDevice) {
        if device
            .fingerprint
            .eq_ignore_ascii_case(&self.config.fingerprint)
        {
            return;
        }
        let event = {
            let mut devices = self.devices.lock();
            let event = match devices
                .iter_mut()
                .find(|known| known.fingerprint == device.fingerprint)
            {
                Some(known) => {
                    *known = device.clone();
                    DiscoveryEvent::Updated(device.clone())
                }
                None => {
                    devices.push(device.clone());
                    DiscoveryEvent::Found(device.clone())
                }
            };
            // One address is one device: a peer that answers there again under
            // another identity — a certificate it regenerated, a fingerprint a
            // plain-HTTP registration only claimed — replaces what was known
            // for that address instead of being offered beside it.
            devices.retain(|known| {
                known.fingerprint == device.fingerprint
                    || known.host != device.host
                    || known.port != device.port
            });
            event
        };
        let _ = self.events.send(event);
    }

    /// Announces this device to `host` and turns the answer into a peer.
    async fn probe(
        &self,
        host: &str,
        port: u16,
        protocol: ProtocolType,
        pin: Option<&str>,
    ) -> Option<DiscoveredDevice> {
        let target = HttpTarget::new(protocol, host, port);
        let client =
            match HttpClient::new(&self.config.identity, &target, pin, Some(DISCOVERY_TIMEOUT)) {
                Ok(client) => client,
                Err(error) => {
                    tracing::debug!("cannot probe {host}:{port}: {error}");
                    return None;
                }
            };
        match client.register(&self.config.register_dto()).await {
            Ok(info) => self.confirmed(info, host, port, protocol, pin.unwrap_or_default()),
            Err(error) => {
                tracing::debug!("{protocol:?} probe of {host}:{port} failed: {error}");
                None
            }
        }
    }

    /// Builds a peer from a register response, ignoring ourselves.
    fn confirmed(
        &self,
        info: DeviceInfoDto,
        host: &str,
        port: u16,
        protocol: ProtocolType,
        claimed_fingerprint: &str,
    ) -> Option<DiscoveredDevice> {
        // Over HTTPS the fingerprint is proven by the handshake; over HTTP all
        // there is to go by is what the peer claimed.
        let fingerprint = match protocol {
            ProtocolType::Https if !claimed_fingerprint.is_empty() => {
                claimed_fingerprint.to_ascii_uppercase()
            }
            ProtocolType::Https => info.fingerprint.to_ascii_uppercase(),
            ProtocolType::Http => {
                let claimed = if info.fingerprint.is_empty() {
                    claimed_fingerprint
                } else {
                    &info.fingerprint
                };
                claimed.to_ascii_uppercase()
            }
        };
        if fingerprint.is_empty() || fingerprint == self.config.fingerprint.to_ascii_uppercase() {
            return None;
        }
        let device = DiscoveredDevice {
            fingerprint,
            alias: info.alias,
            version: info.version,
            device_model: info.device_model,
            device_type: info.device_type,
            download: info.download,
            protocol,
            host: host.to_string(),
            port,
            last_seen: SystemTime::now(),
        };
        self.store(device.clone());
        Some(device)
    }
}

/// Drops every peer that was not seen since `since`, returning how many were
/// dropped.
fn forget_unseen(devices: &mut Vec<DiscoveredDevice>, since: SystemTime) -> usize {
    let known = devices.len();
    devices.retain(|device| device.last_seen >= since);
    known - devices.len()
}

/// Strips the brackets a user may type around an IPv6 address.
fn strip_brackets(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

/// The IPv4 addresses of every non-loopback interface.
pub fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .into_iter()
            .filter(|interface| !interface.is_loopback())
            .filter_map(|interface| match interface.addr {
                if_addrs::IfAddr::V4(address) => Some(address.ip),
                if_addrs::IfAddr::V6(_) => None,
            })
            .collect(),
        Err(error) => {
            tracing::warn!("cannot enumerate network interfaces: {error}");
            Vec::new()
        }
    }
}

/// All multicast sockets, one per interface and family.
type Sockets = Vec<(Arc<UdpSocket>, SocketAddr)>;

/// Binds and joins the multicast group on every usable interface.
fn bind_multicast_sockets(port: u16) -> Sockets {
    let mut sockets: Sockets = Vec::new();
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            tracing::warn!("cannot enumerate network interfaces: {error}");
            return sockets;
        }
    };

    for interface in interfaces.iter().filter(|i| !i.is_loopback()) {
        match &interface.addr {
            if_addrs::IfAddr::V4(address) => match bind_multicast_v4(port, address.ip) {
                Ok(socket) => {
                    let target = SocketAddr::new(IpAddr::V4(DEFAULT_MULTICAST_GROUP), port);
                    sockets.push((Arc::new(socket), target));
                }
                Err(error) => tracing::debug!(
                    "no IPv4 multicast on {} ({}): {error}",
                    interface.name,
                    address.ip
                ),
            },
            if_addrs::IfAddr::V6(_) => {}
        }
    }

    // IPv6 needs one socket per interface, joined by interface index.
    #[cfg(unix)]
    for (name, index) in interface_indices() {
        if name == "lo" {
            continue;
        }
        match bind_multicast_v6(port, index) {
            Ok(socket) => {
                let target =
                    SocketAddr::new(IpAddr::V6(DEFAULT_MULTICAST_GROUP_V6), port).to_string();
                let target = target
                    .parse::<SocketAddr>()
                    .expect("a formed address always parses");
                let target = match target {
                    SocketAddr::V6(address) => {
                        SocketAddr::V6(std::net::SocketAddrV6::new(*address.ip(), port, 0, index))
                    }
                    address => address,
                };
                sockets.push((Arc::new(socket), target));
            }
            Err(error) => tracing::debug!("no IPv6 multicast on {name}: {error}"),
        }
    }

    sockets
}

/// The interface names and indices the platform reports.
#[cfg(unix)]
fn interface_indices() -> Vec<(String, u32)> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut seen = HashMap::new();
    for interface in interfaces {
        if seen.contains_key(&interface.name) {
            continue;
        }
        let name = interface.name.clone();
        if let Ok(index) = std::ffi::CString::new(name.clone()) {
            // SAFETY: the pointer is valid for the duration of the call.
            let index = unsafe { libc::if_nametoindex(index.as_ptr()) };
            if index != 0 {
                seen.insert(name, index);
            }
        }
    }
    seen.into_iter().collect()
}

fn bind_multicast_v4(port: u16, interface: Ipv4Addr) -> Result<UdpSocket> {
    let socket = multicast_socket(socket2::Domain::IPV4)?;
    // Binding the wildcard address is what makes the socket receive datagrams
    // on platforms that match the destination against the bound address.
    socket.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port).into())?;
    socket.join_multicast_v4(&DEFAULT_MULTICAST_GROUP, &interface)?;
    // Without this the routing table decides the egress interface, which would
    // send every socket's announcements through the same one.
    socket.set_multicast_if_v4(&interface)?;
    // Loopback stays on so several instances on one host see each other; our
    // own announcements are filtered by fingerprint.
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into()).context("wrapping the IPv4 multicast socket")
}

#[cfg(unix)]
fn bind_multicast_v6(port: u16, index: u32) -> Result<UdpSocket> {
    let socket = multicast_socket(socket2::Domain::IPV6)?;
    socket.set_only_v6(true)?;
    socket.bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port).into())?;
    socket.join_multicast_v6(&DEFAULT_MULTICAST_GROUP_V6, index)?;
    socket.set_multicast_if_v6(index)?;
    socket.set_multicast_loop_v6(true)?;
    socket.set_multicast_hops_v6(1)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into()).context("wrapping the IPv6 multicast socket")
}

/// A UDP socket shared by every instance on the host.
fn multicast_socket(domain: socket2::Domain) -> Result<socket2::Socket> {
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // All sockets of all instances share the same port.
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    socket.set_reuse_port(true)?;
    Ok(socket)
}

/// Receives announcements and answers each one over HTTP.
async fn receive_loop(
    state: Arc<State>,
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    shutdown: CancellationToken,
) {
    let mut buffer = vec![0u8; RECEIVE_BUFFER_SIZE];
    let mut consecutive_errors = 0u32;
    loop {
        let received = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            received = socket.recv_from(&mut buffer) => received,
        };
        let (len, remote) = match received {
            Ok(received) => {
                consecutive_errors = 0;
                received
            }
            Err(error) => {
                consecutive_errors += 1;
                tracing::debug!("multicast receive on {target} failed: {error}");
                if consecutive_errors >= 10 {
                    let _ = state.events.send(DiscoveryEvent::MulticastFailed {
                        error: error.to_string(),
                    });
                    return;
                }
                continue;
            }
        };
        let message: MulticastMessage = match serde_json::from_slice(&buffer[..len]) {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!("ignoring a malformed announcement from {remote}: {error}");
                continue;
            }
        };
        // Loopback is enabled, so our own announcements come back as well.
        if message
            .fingerprint
            .eq_ignore_ascii_case(&state.config.fingerprint)
        {
            continue;
        }
        let state = state.clone();
        tokio::spawn(async move {
            answer_announcement(state, remote, message).await;
        });
    }
}

/// Registers with a peer that just announced itself.
async fn answer_announcement(state: Arc<State>, remote: SocketAddr, message: MulticastMessage) {
    let host = match remote {
        SocketAddr::V6(address) if address.scope_id() != 0 => {
            format!("{}%{}", address.ip(), address.scope_id())
        }
        address => address.ip().to_string(),
    };
    // Over HTTPS the announced fingerprint is what the certificate must prove.
    let pin = match message.protocol {
        ProtocolType::Https => Some(message.fingerprint.as_str()),
        ProtocolType::Http => None,
    };
    if state
        .probe(&host, message.port, message.protocol, pin)
        .await
        .is_none()
    {
        tracing::debug!("no answer from {host}:{}", message.port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(fingerprint: &str, host: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            fingerprint: fingerprint.to_string(),
            alias: "peer".to_string(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: None,
            device_type: Some(DeviceType::Desktop),
            download: false,
            protocol: ProtocolType::Https,
            host: host.to_string(),
            port: 53317,
            last_seen: SystemTime::now(),
        }
    }

    fn state() -> (Arc<State>, mpsc::UnboundedReceiver<DiscoveryEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let state = Arc::new(State {
            config: DiscoveryConfig {
                port: 53317,
                protocol: ProtocolType::Https,
                alias: "self".to_string(),
                device_model: None,
                device_type: Some(DeviceType::Desktop),
                fingerprint: "SELF".to_string(),
                download: false,
                identity: Identity::generate().unwrap(),
            },
            devices: Mutex::new(Vec::new()),
            sockets: Mutex::new(Vec::new()),
            events,
        });
        (state, receiver)
    }

    #[test]
    fn storing_a_peer_twice_updates_instead_of_duplicating() {
        let (state, mut events) = state();
        state.store(device("A", "10.0.0.2"));
        assert!(matches!(events.try_recv(), Ok(DiscoveryEvent::Found(_))));
        state.store(device("A", "10.0.0.9"));
        assert!(matches!(events.try_recv(), Ok(DiscoveryEvent::Updated(_))));
        let devices = state.devices.lock();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].host, "10.0.0.9");
    }

    #[test]
    fn a_scan_forgets_the_peers_that_did_not_answer_it() {
        let (state, _events) = state();
        let scan_started = SystemTime::now();

        // Seen before the scan began and silent since: gone.
        let mut stale = device("STALE", "10.0.0.2");
        stale.last_seen = scan_started - Duration::from_secs(30);
        state.store(stale);
        // Answered the scan: kept, wherever it was found — an announcement and
        // a probe both count, since both stamp the sighting.
        state.store(device("ANSWERED", "10.0.0.3"));

        assert_eq!(forget_unseen(&mut state.devices.lock(), scan_started), 1);
        let devices = state.devices.lock();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].fingerprint, "ANSWERED");
    }

    #[test]
    fn this_device_never_becomes_a_peer_of_itself() {
        let (state, mut events) = state();
        // A phone's interfaces share a subnet, so a scan reaches this device's
        // own address, and the registration that comes back is its own.
        state.store(device("SELF", "10.0.0.5"));
        assert!(state.devices.lock().is_empty());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn one_address_holds_one_device() {
        let (state, mut events) = state();
        state.store(device("OLD", "10.0.0.2"));
        assert!(matches!(events.try_recv(), Ok(DiscoveryEvent::Found(_))));

        // The same device comes back under another identity — a certificate it
        // regenerated, or a fingerprint a plain-HTTP answer only claimed — and
        // the address is what says it is the same device.
        state.store(device("NEW", "10.0.0.2"));
        assert!(matches!(events.try_recv(), Ok(DiscoveryEvent::Found(_))));
        let devices = state.devices.lock();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].fingerprint, "NEW");

        // Another address is another device, and keeps the first company.
        drop(devices);
        state.store(device("ELSEWHERE", "10.0.0.3"));
        assert_eq!(state.devices.lock().len(), 2);
    }

    #[test]
    fn our_own_registration_response_is_not_a_peer() {
        let (state, _events) = state();
        let info = DeviceInfoDto {
            alias: "self".to_string(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: None,
            device_type: None,
            fingerprint: "SELF".to_string(),
            download: false,
        };
        assert!(
            state
                .confirmed(info, "127.0.0.1", 53317, ProtocolType::Http, "")
                .is_none()
        );
    }

    #[test]
    fn https_peers_are_keyed_by_the_fingerprint_their_certificate_proves() {
        let (state, _events) = state();
        let info = DeviceInfoDto {
            alias: "liar".to_string(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: None,
            device_type: None,
            fingerprint: "claimed-in-body".to_string(),
            download: false,
        };
        let device = state
            .confirmed(info, "10.0.0.2", 53317, ProtocolType::Https, "proven")
            .unwrap();
        assert_eq!(device.fingerprint, "PROVEN");
    }

    #[tokio::test]
    async fn announcements_are_answered_over_http_and_end_up_in_the_store() {
        use crate::server::Server;
        use crate::server::ServerConfig;

        // A server that accepts anything, standing in for the remote peer.
        let identity = Identity::generate().unwrap();
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let server = Server::start(ServerConfig {
            port: 0,
            identity: Some(identity.clone()),
            http_fingerprint: String::new(),
            alias: "responder".to_string(),
            device_model: None,
            device_type: Some(DeviceType::Desktop),
            pin: None,
            verify_checksums: false,
            download_enabled: false,
            events: events_tx,
        })
        .await
        .unwrap();

        let (discovery_events, _receiver) = mpsc::unbounded_channel();
        // This device's own identity, so the fingerprint it claims matches the
        // certificate it presents — otherwise the peer discards the
        // registration, which the negative test in `server` pins down.
        let own_identity = Identity::generate().unwrap();
        let discovery = Discovery::start(
            DiscoveryConfig {
                port: 0,
                protocol: ProtocolType::Https,
                alias: "prober".to_string(),
                device_model: None,
                device_type: Some(DeviceType::Desktop),
                fingerprint: own_identity.fingerprint.clone(),
                download: false,
                identity: own_identity,
            },
            discovery_events,
        )
        .await
        .unwrap();

        let device = discovery
            .probe_host("127.0.0.1", server.port())
            .await
            .expect("the peer answers a register request");
        assert_eq!(device.fingerprint, identity.fingerprint);
        assert_eq!(device.alias, "responder");
        assert_eq!(device.port, server.port());
        assert!(matches!(
            events_rx.try_recv(),
            Ok(crate::server::ServerEvent::Discovered { .. })
        ));

        discovery.stop().await;
        server.shutdown().await;
    }
}
