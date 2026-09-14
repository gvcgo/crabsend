//! Application state: the transfer server, discovery and the transfer sessions
//! the UI renders.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use crabsend_core::client::ClientError;
use crabsend_core::client::HttpClient;
use crabsend_core::client::HttpTarget;
use crabsend_core::client::PrepareOutcome;
use crabsend_core::crypto::Identity;
use crabsend_core::discovery::DiscoveredDevice;
use crabsend_core::discovery::Discovery;
use crabsend_core::discovery::DiscoveryConfig;
use crabsend_core::discovery::DiscoveryEvent;
use crabsend_core::discovery::local_ipv4_addresses;
use crabsend_core::model::DeviceInfoDto;
use crabsend_core::model::DeviceType;
use crabsend_core::model::FileDto;
use crabsend_core::model::FileMetadata;
use crabsend_core::model::PrepareUploadRequest;
use crabsend_core::model::ProtocolType;
use crabsend_core::model::RegisterDto;
use crabsend_core::server::PeerIdentity;
use crabsend_core::server::Server;
use crabsend_core::server::ServerConfig;
use crabsend_core::server::ServerEvent;
use crabsend_core::server::SessionOutcome;
use crabsend_core::server::UploadDecision;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use tauri::AppHandle;
use tauri::Emitter;
use tauri::Manager;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::pairing::PAYLOAD_VERSION;
use crate::pairing::PairingPayload;
use crate::pairing::PairingQr;
use crate::pairing::qr_svg;
use crate::settings::Settings;

/// How many files of one session are uploaded at once.
const UPLOAD_CONCURRENCY: usize = 2;

/// How often one file may be retried after a checksum mismatch.
const MAX_UPLOAD_ATTEMPTS: usize = 3;

/// History entries kept.
const MAX_HISTORY: usize = 100;

/// How long one address of a pairing code may take to answer.
const PAIR_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the devices paired by QR code are remembered.
const PEERS_FILE: &str = "peers.json";

/// The whole state the UI renders, pushed on every structural change.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub settings: Settings,
    pub device: DeviceInfoOut,
    pub server: ServerStatus,
    pub devices: Vec<DeviceOut>,
    pub sessions: Vec<Session>,
    pub incoming: Option<IncomingRequest>,
    pub history: Vec<HistoryEntry>,
    pub pairing: PairingSupport,
    /// Whether this platform has a file manager to show a received file in.
    /// A phone has none: its file managers cannot even see the directory the
    /// files are saved to, so the interface must not offer the action.
    pub can_reveal_files: bool,
    /// Whether this platform can ask the user for a directory. Android's file
    /// dialogs cannot, so the buttons that would are hidden there.
    pub can_pick_folder: bool,
}

/// What pairing can do on the platform this build runs on.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PairingSupport {
    /// Reading a code needs a camera, which only the mobile build can open.
    pub can_scan: bool,
    /// Every platform can show its own code for another device to scan.
    pub can_show_qr: bool,
}

impl PairingSupport {
    fn current() -> Self {
        Self {
            can_scan: cfg!(any(target_os = "android", target_os = "ios")),
            can_show_qr: true,
        }
    }
}

/// This device, as shown in the header.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfoOut {
    pub alias: String,
    pub fingerprint: String,
    pub device_model: Option<String>,
    pub device_type: DeviceType,
    pub protocol: ProtocolType,
    pub port: u16,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub running: bool,
    pub protocol: ProtocolType,
    pub port: u16,
    pub error: Option<String>,
}

/// A peer the user can send to.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeviceOut {
    pub fingerprint: String,
    pub alias: String,
    pub device_model: Option<String>,
    pub device_type: DeviceType,
    pub protocol: ProtocolType,
    pub host: String,
    pub port: u16,
    pub download: bool,
    pub last_seen: u64,
    /// True when a pairing code proved this device's identity: such a device is
    /// kept between runs instead of being forgotten with the discoveries.
    pub paired: bool,
}

/// A device this one was paired with, as stored between runs.
///
/// The address is what the pairing code carried, so it is a hint rather than a
/// fact: a peer that moved is found again by a scan, which refreshes it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedPeer {
    pub fingerprint: String,
    pub alias: String,
    pub device_model: Option<String>,
    pub device_type: DeviceType,
    pub protocol: ProtocolType,
    pub host: String,
    pub port: u16,
    /// When this device last answered: the moment the code was scanned, then
    /// every scan that reached it again, in milliseconds since the epoch.
    pub last_seen: u64,
}

impl PairedPeer {
    /// The payload that reaches this device again, for a refresh.
    fn payload(&self) -> PairingPayload {
        PairingPayload {
            v: PAYLOAD_VERSION,
            protocol: self.protocol,
            addresses: vec![self.host.clone()],
            port: self.port,
            fingerprint: self.fingerprint.clone(),
            alias: self.alias.clone(),
            device_model: self.device_model.clone(),
            device_type: Some(self.device_type),
        }
    }
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Preparing,
    Waiting,
    Active,
    PinRequired,
    Done,
    Failed,
    Cancelled,
    Declined,
    Busy,
}

impl SessionStatus {
    /// Whether the session will not change any more.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SessionStatus::Done
                | SessionStatus::Failed
                | SessionStatus::Cancelled
                | SessionStatus::Declined
                | SessionStatus::Busy
        )
    }
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Pending,
    Hashing,
    Active,
    Done,
    Failed,
    Skipped,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TransferFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub transferred: u64,
    pub status: FileStatus,
    pub error: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TransferPeer {
    pub alias: String,
    pub fingerprint: String,
    pub device_model: Option<String>,
    pub device_type: DeviceType,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub direction: Direction,
    pub peer: TransferPeer,
    pub status: SessionStatus,
    pub files: Vec<TransferFile>,
    pub error: Option<String>,
    pub started_at: u64,
    /// Internal: everything the session needs to keep working.
    #[serde(skip)]
    pub job: Option<SendJob>,
    #[serde(skip)]
    pub saved_dir: Option<PathBuf>,
}

/// Everything a send session needs beyond its public view.
#[derive(Clone)]
pub struct SendJob {
    target: DeviceOut,
    remote_session_id: Option<String>,
    cancel: CancellationToken,
    /// Where each file of the session is read from, by file id.
    paths: HashMap<String, PathBuf>,
}

/// A file of a send session, paired with the id the protocol knows it by.
struct PlannedFile {
    file: SendFile,
    id: String,
}

impl PlannedFile {
    fn new(file: SendFile) -> Self {
        Self {
            id: crabsend_core::crypto::random_id(),
            file,
        }
    }
}

/// A file offered by a peer, as shown in the incoming dialog.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OfferedFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
}

/// What the UI needs to ask the user about an incoming transfer.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IncomingRequest {
    pub session_id: String,
    pub peer: TransferPeer,
    pub files: Vec<OfferedFile>,
    pub total_size: u64,
    pub pin_protected: bool,
}

/// The pending decision, kept out of the snapshot.
pub struct Incoming {
    pub request: IncomingRequest,
    pub decision: Option<oneshot::Sender<UploadDecision>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub direction: Direction,
    pub peer_alias: String,
    pub peer_fingerprint: String,
    pub file_count: usize,
    pub total_size: u64,
    pub status: SessionStatus,
    pub at: u64,
    pub saved_dir: Option<String>,
}

/// A file the user picked, on its way to a peer.
#[derive(Clone, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendFile {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub sha256: Option<String>,
    pub modified: Option<String>,
    pub accessed: Option<String>,
}

/// Where state changes are published.
///
/// The window is the only consumer in the application, but keeping the
/// transport behind one type lets the whole state machine run without a
/// window, which is what the interop test does.
enum Notifier {
    Window(AppHandle),
    /// Nothing is listening; the state is read through [`AppState::snapshot`].
    Silent,
}

impl Notifier {
    fn emit<S: Serialize + Clone>(&self, event: &str, payload: S) {
        match self {
            Notifier::Window(app) => {
                if let Err(error) = app.emit(event, payload) {
                    tracing::debug!("cannot reach the window: {error}");
                }
            }
            Notifier::Silent => {}
        }
    }
}

/// The application.
pub struct AppState {
    notifier: Notifier,
    config_dir: PathBuf,
    pub identity: Identity,
    pub settings: Mutex<Settings>,
    server: Mutex<Option<Arc<Server>>>,
    discovery: Mutex<Option<Arc<Discovery>>>,
    server_status: Mutex<ServerStatus>,
    sessions: Mutex<Vec<Session>>,
    incoming: Mutex<Option<Incoming>>,
    history: Mutex<Vec<HistoryEntry>>,
    /// Devices paired by QR code, kept across runs in `peers.json`.
    paired: Mutex<Vec<PairedPeer>>,
}

impl AppState {
    /// Loads the settings and the device identity; the caller starts serving
    /// with [`AppState::restart`].
    pub fn new(app: AppHandle, config_dir: PathBuf) -> Result<Self> {
        let download_dir = platform_download_dir(&app);
        Self::build(Notifier::Window(app), config_dir, download_dir)
    }

    /// Builds a state that publishes nothing, for headless embedding and tests.
    pub fn new_headless(config_dir: PathBuf) -> Result<Self> {
        Self::build(Notifier::Silent, config_dir, None)
    }

    fn build(
        notifier: Notifier,
        config_dir: PathBuf,
        platform_download_dir: Option<PathBuf>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        let settings_path = config_dir.join("settings.json");
        let mut settings = Settings::load(&settings_path).normalized();
        if !settings_path.exists()
            && let Some(dir) = platform_download_dir
        {
            settings.download_dir = dir;
        }
        // A settings file written while received files still landed in the
        // application's files directory keeps pointing there — the one place
        // Android hides from every file manager, which is why the files could
        // not be found on the phone. Move it along with them, files included,
        // so what was already received becomes findable too.
        if let Some(shared) = crate::settings::android_media_dir(&settings.download_dir) {
            let previous = settings.download_dir.clone();
            settings.download_dir = shared;
            if let Err(error) = settings.save(&settings_path) {
                tracing::warn!("cannot persist the moved download directory: {error:#}");
            }
            move_received_files(&previous, &settings.download_dir);
        }
        let identity = load_or_create_identity(&config_dir)?;
        let paired = load_peers(&config_dir.join(PEERS_FILE));

        Ok(Self {
            notifier,
            config_dir,
            identity,
            settings: Mutex::new(settings),
            server: Mutex::new(None),
            discovery: Mutex::new(None),
            server_status: Mutex::new(ServerStatus {
                running: false,
                protocol: ProtocolType::Https,
                port: 0,
                error: None,
            }),
            sessions: Mutex::new(Vec::new()),
            incoming: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            paired: Mutex::new(paired),
        })
    }

    pub fn settings_path(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    /// Where the devices paired by QR code are remembered.
    fn peers_path(&self) -> PathBuf {
        self.config_dir.join(PEERS_FILE)
    }

    /// The state as the UI sees it.
    pub fn snapshot(&self) -> Snapshot {
        let settings = self.settings.lock().clone();
        let status = self.server_status.lock().clone();
        let mut sessions = self.sessions.lock().clone();
        sessions.reverse();
        Snapshot {
            device: DeviceInfoOut {
                alias: settings.alias.clone(),
                fingerprint: self.identity.fingerprint.clone(),
                device_model: settings.device_model.clone(),
                device_type: settings.device_type,
                protocol: status.protocol,
                port: status.port,
            },
            server: status,
            devices: self.devices(),
            sessions,
            incoming: self
                .incoming
                .lock()
                .as_ref()
                .map(|incoming| incoming.request.clone()),
            settings,
            history: self.history.lock().clone(),
            pairing: PairingSupport::current(),
            can_reveal_files: cfg!(not(any(target_os = "android", target_os = "ios"))),
            can_pick_folder: cfg!(not(any(target_os = "android", target_os = "ios"))),
        }
    }

    /// Every peer the user can send to: what discovery has seen, followed by
    /// the paired devices that are not in that list, so a peer reachable
    /// without multicast is still offered. A fresh sighting wins over the
    /// address a pairing code carried.
    pub fn devices(&self) -> Vec<DeviceOut> {
        let paired = self.paired.lock().clone();
        let mut devices: Vec<DeviceOut> = self
            .discovery
            .lock()
            .as_ref()
            .map(|discovery| discovery.devices())
            .unwrap_or_default()
            .into_iter()
            .map(device_out)
            .collect();
        for device in &mut devices {
            device.paired = paired
                .iter()
                .any(|peer| peer.fingerprint == device.fingerprint);
        }
        for peer in &paired {
            if devices
                .iter()
                .any(|device| device.fingerprint == peer.fingerprint)
            {
                continue;
            }
            devices.push(paired_device(peer));
        }
        devices
    }

    /// Pushes the whole state to the UI.
    pub fn emit_state(&self) {
        self.notifier.emit("state", self.snapshot());
    }

    fn emit_progress(&self, session_id: &str, file_id: &str, transferred: u64) {
        let payload = ProgressPayload {
            session_id: session_id.to_string(),
            file_id: file_id.to_string(),
            transferred,
        };
        self.notifier.emit("progress", payload);
    }

    /// Applies `change` to one session; the caller decides whether to emit.
    fn with_session<R>(&self, id: &str, change: impl FnOnce(&mut Session) -> R) -> Option<R> {
        let mut sessions = self.sessions.lock();
        sessions
            .iter_mut()
            .find(|session| session.id == id)
            .map(change)
    }

    /// Applies `change` to one file of one session and reports the result.
    fn with_file<R>(
        &self,
        session_id: &str,
        file_id: &str,
        change: impl FnOnce(&mut TransferFile) -> R,
    ) -> Option<R> {
        self.with_session(session_id, |session| {
            session
                .files
                .iter_mut()
                .find(|file| file.id == file_id)
                .map(change)
        })
        .flatten()
    }

    /// Records the end of a session and moves it into the history.
    fn finish_session(&self, id: &str, status: SessionStatus, error: Option<String>) {
        let entry = {
            let mut sessions = self.sessions.lock();
            let Some(session) = sessions.iter_mut().find(|session| session.id == id) else {
                return;
            };
            session.status = status;
            session.error = error.clone();
            if status == SessionStatus::Cancelled && session.job.is_some() {
                if let Some(job) = &session.job {
                    job.cancel.cancel();
                }
            }
            HistoryEntry {
                id: session.id.clone(),
                direction: session.direction,
                peer_alias: session.peer.alias.clone(),
                peer_fingerprint: session.peer.fingerprint.clone(),
                file_count: session.files.len(),
                total_size: session.files.iter().map(|file| file.size).sum(),
                status,
                at: now_ms(),
                saved_dir: session
                    .saved_dir
                    .as_ref()
                    .map(|dir| dir.display().to_string()),
            }
        };
        self.history.lock().insert(0, entry);
        self.history.lock().truncate(MAX_HISTORY);
        self.emit_state();
    }

    // -----------------------------------------------------------------------
    // Server and discovery
    // -----------------------------------------------------------------------

    /// (Re)starts the transfer server and multicast discovery.
    pub async fn restart(self: &Arc<Self>) {
        self.stop().await;

        let settings = self.settings.lock().clone();
        let (events, receiver) = mpsc::unbounded_channel();
        let config = ServerConfig {
            port: settings.port,
            identity: settings.encryption.then(|| self.identity.clone()),
            // The certificate fingerprint identifies this device in both modes,
            // so a peer that switches to HTTPS still recognises it.
            http_fingerprint: self.identity.fingerprint.clone(),
            alias: settings.alias.clone(),
            device_model: settings.device_model.clone(),
            device_type: Some(settings.device_type),
            pin: settings.pin.clone(),
            // Always verify the checksum a sender provides.
            verify_checksums: true,
            // The browser download API is not implemented; peers are told so.
            download_enabled: false,
            events,
        };

        match Server::start(config).await {
            Ok(server) => {
                let port = server.port();
                let protocol = server.protocol();
                *self.server_status.lock() = ServerStatus {
                    running: true,
                    protocol,
                    port,
                    error: None,
                };
                tokio::spawn(pump_server_events(self.clone(), receiver));
                *self.server.lock() = Some(Arc::new(server));
                self.start_discovery(port, protocol).await;
            }
            Err(error) => {
                tracing::error!("cannot start the transfer server: {error:#}");
                *self.server_status.lock() = ServerStatus {
                    running: false,
                    protocol: if settings.encryption {
                        ProtocolType::Https
                    } else {
                        ProtocolType::Http
                    },
                    port: settings.port,
                    error: Some(format!("{error:#}")),
                };
            }
        }
        self.emit_state();
    }

    /// Stops the server and discovery.
    pub async fn stop(&self) {
        // Taken out of the locks first: a `parking_lot` guard must not be held
        // across an await.
        let server = self.server.lock().take();
        if let Some(server) = server {
            server.shutdown().await;
        }
        let discovery = self.discovery.lock().take();
        if let Some(discovery) = discovery {
            discovery.stop().await;
        }
    }

    async fn start_discovery(self: &Arc<Self>, port: u16, protocol: ProtocolType) {
        let settings = self.settings.lock().clone();
        let (events, mut receiver) = mpsc::unbounded_channel();
        let config = DiscoveryConfig {
            port,
            protocol,
            alias: settings.alias.clone(),
            device_model: settings.device_model.clone(),
            device_type: Some(settings.device_type),
            fingerprint: self.identity.fingerprint.clone(),
            download: false,
            identity: self.identity.clone(),
        };
        match Discovery::start(config, events).await {
            Ok(discovery) => {
                let discovery = Arc::new(discovery);
                *self.discovery.lock() = Some(discovery.clone());
                let state = self.clone();
                tokio::spawn(async move {
                    while let Some(event) = receiver.recv().await {
                        match event {
                            DiscoveryEvent::Found(_) | DiscoveryEvent::Updated(_) => {
                                state.emit_state();
                            }
                            DiscoveryEvent::MulticastFailed { error } => {
                                tracing::warn!("multicast discovery unavailable: {error}");
                            }
                        }
                    }
                });
                // Make this device visible right away.
                let announcement = discovery.clone();
                tokio::spawn(async move { announcement.announce().await });
            }
            Err(error) => {
                tracing::warn!("discovery is unavailable: {error:#}");
            }
        }
    }

    /// Announces this device, scans the local subnets and refreshes the
    /// addresses of the paired devices.
    pub async fn scan(self: &Arc<Self>) {
        let Some(discovery) = self.discovery.lock().clone() else {
            return;
        };
        let state = self.clone();
        tokio::spawn(async move {
            let announce = discovery.clone();
            let scan = discovery.clone();
            let refresh = state.clone();
            let (_, found, ()) = tokio::join!(
                announce.announce(),
                async move { scan.scan_subnet(&local_ipv4_addresses()).await },
                async move { refresh.refresh_paired().await },
            );
            tracing::info!("scan finished with {} peers", found.len());
            state.emit_state();
        });
    }

    /// Registers with one host the user typed in, with or without a port.
    pub async fn add_device(self: &Arc<Self>, host: &str) -> Result<()> {
        let (host, port) = split_host_port(host)?;
        let discovery = self
            .discovery
            .lock()
            .clone()
            .context("discovery is not running")?;
        match discovery.probe_host(&host, port).await {
            Some(device) => {
                tracing::info!("added {} at {}", device.alias, device.host);
                self.emit_state();
                Ok(())
            }
            None => anyhow::bail!("no Crabsend or LocalSend device answered at {host}:{port}"),
        }
    }

    /// Forgets every discovered peer.
    pub fn clear_devices(&self) {
        if let Some(discovery) = self.discovery.lock().as_ref() {
            discovery.clear();
        }
        self.emit_state();
    }

    // -----------------------------------------------------------------------
    // Pairing
    // -----------------------------------------------------------------------

    /// The code another device scans to pair with this one.
    pub async fn pairing_qr(&self) -> Result<PairingQr> {
        let status = self.server_status.lock().clone();
        anyhow::ensure!(status.running, "the transfer server is not running");
        let settings = self.settings.lock().clone();
        // Every address is offered because a multi-homed host cannot know which
        // of them the scanner can reach; it tries them all at once.
        let addresses = tokio::task::spawn_blocking(local_ipv4_addresses)
            .await
            .context("collecting the local addresses")?
            .into_iter()
            .map(|address| address.to_string())
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !addresses.is_empty(),
            "this device has no network address to put in the code"
        );
        let payload = PairingPayload {
            v: PAYLOAD_VERSION,
            protocol: status.protocol,
            addresses,
            port: status.port,
            fingerprint: self.identity.fingerprint.clone(),
            alias: settings.alias.clone(),
            device_model: settings.device_model.clone(),
            device_type: Some(settings.device_type),
        }
        .to_json()?;
        Ok(PairingQr {
            svg: qr_svg(&payload)?,
            payload,
        })
    }

    /// Pairs with the device whose code was scanned.
    ///
    /// The peer is dialled with the fingerprint from the code pinned to its
    /// certificate, so over HTTPS a device that answers in its place is refused
    /// during the handshake, before any request or file reaches it.
    pub async fn pair_from_qr(&self, payload: &str) -> Result<DeviceOut> {
        let payload = PairingPayload::parse(payload)?;
        anyhow::ensure!(
            payload.fingerprint != self.identity.fingerprint,
            "that code belongs to this device"
        );
        let (info, host) = self.register_pinned(&payload).await?;
        let peer = PairedPeer {
            fingerprint: payload.fingerprint,
            alias: if info.alias.trim().is_empty() {
                payload.alias
            } else {
                info.alias
            },
            device_model: info.device_model.or(payload.device_model),
            device_type: info
                .device_type
                .or(payload.device_type)
                .unwrap_or_default(),
            protocol: payload.protocol,
            host,
            port: payload.port,
            last_seen: now_ms(),
        };
        {
            let mut paired = self.paired.lock();
            match paired
                .iter_mut()
                .find(|known| known.fingerprint == peer.fingerprint)
            {
                Some(known) => *known = peer.clone(),
                None => paired.push(peer.clone()),
            }
        }
        self.save_paired();
        self.emit_state();
        tracing::info!("paired with {} at {}:{}", peer.alias, peer.host, peer.port);
        Ok(paired_device(&peer))
    }

    /// Forgets a device this one was paired with.
    pub fn unpair(&self, fingerprint: &str) -> Result<()> {
        let removed = {
            let mut paired = self.paired.lock();
            let before = paired.len();
            paired.retain(|peer| peer.fingerprint != fingerprint);
            paired.len() != before
        };
        anyhow::ensure!(removed, "no paired device has that fingerprint");
        self.save_paired();
        self.emit_state();
        Ok(())
    }

    /// Refreshes the address and the details of every paired device.
    ///
    /// Best effort, and deliberately not called at startup: a peer that is
    /// switched off would only add a timeout to every launch. [`AppState::scan`]
    /// runs it, because that is also when a peer that moved can be reached
    /// again; the caller publishes the result.
    async fn refresh_paired(&self) {
        let mut changed = false;
        // Cloned out of the lock: a `parking_lot` guard must not be held
        // across an await.
        let peers = self.paired.lock().clone();
        for peer in peers {
            let Ok((info, host)) = self.register_pinned(&peer.payload()).await else {
                continue;
            };
            let mut paired = self.paired.lock();
            let Some(known) = paired
                .iter_mut()
                .find(|known| known.fingerprint == peer.fingerprint)
            else {
                continue;
            };
            known.host = host;
            known.last_seen = now_ms();
            if !info.alias.trim().is_empty() {
                known.alias = info.alias;
            }
            if let Some(model) = info.device_model {
                known.device_model = Some(model);
            }
            if let Some(device_type) = info.device_type {
                known.device_type = device_type;
            }
            changed = true;
        }
        if changed {
            self.save_paired();
        }
    }

    /// Registers with a peer whose address and fingerprint came from a code,
    /// trying every address it carries at once.
    ///
    /// Over HTTPS the pin is enforced during the handshake. Over plain HTTP
    /// there is no certificate to pin, so the code only rules out reaching the
    /// wrong device, not a peer that lies about its fingerprint.
    async fn register_pinned(&self, payload: &PairingPayload) -> Result<(DeviceInfoDto, String)> {
        let info = local_info(self);
        let mut pending = FuturesUnordered::new();
        for address in &payload.addresses {
            // Owned copies: the future outlives the loop iteration.
            let address = address.clone();
            let expected = payload.fingerprint.clone();
            let target = HttpTarget::new(payload.protocol, address.clone(), payload.port);
            // Over HTTPS the pin is checked in the handshake; over HTTP there is
            // no certificate to compare it with.
            let pin = match payload.protocol {
                ProtocolType::Https => Some(payload.fingerprint.clone()),
                ProtocolType::Http => None,
            };
            let info = info.clone();
            pending.push(async move {
                let client =
                    HttpClient::new(&self.identity, &target, pin.as_deref(), Some(PAIR_TIMEOUT))?;
                let answer = client.register(&info).await?;
                anyhow::ensure!(
                    answer.fingerprint.is_empty()
                        || answer.fingerprint.eq_ignore_ascii_case(&expected),
                    "{address} answered with another device's fingerprint"
                );
                Ok::<_, anyhow::Error>((answer, address))
            });
        }
        let mut failure = None;
        while let Some(answer) = pending.next().await {
            match answer {
                Ok(found) => return Ok(found),
                Err(error) => failure = Some(error),
            }
        }
        Err(failure.unwrap_or_else(|| anyhow::anyhow!("the code carries no address to try")))
    }

    fn save_paired(&self) {
        let peers = self.paired.lock().clone();
        if let Err(error) = write_peers(&self.peers_path(), &peers) {
            tracing::error!("cannot persist paired devices: {error:#}");
        }
    }

    // -----------------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------------

    /// Starts a send session and returns its id.
    pub fn start_send(
        self: &Arc<Self>,
        target: &str,
        files: Vec<SendFile>,
        pin: Option<String>,
    ) -> Result<String> {
        anyhow::ensure!(!files.is_empty(), "pick at least one file");
        let device = self
            .devices()
            .into_iter()
            .find(|device| device.fingerprint == target)
            .with_context(|| format!("device {target} is not in the device list"))?;

        let session_id = crabsend_core::crypto::random_id();
        let cancel = CancellationToken::new();
        let planned: Vec<PlannedFile> = files.into_iter().map(PlannedFile::new).collect();
        let session = Session {
            id: session_id.clone(),
            direction: Direction::Send,
            peer: TransferPeer {
                alias: device.alias.clone(),
                fingerprint: device.fingerprint.clone(),
                device_model: device.device_model.clone(),
                device_type: device.device_type,
            },
            status: SessionStatus::Preparing,
            files: planned
                .iter()
                .map(|planned| TransferFile {
                    id: planned.id.clone(),
                    name: planned.file.name.clone(),
                    size: planned.file.size,
                    mime: planned.file.mime.clone(),
                    transferred: 0,
                    status: FileStatus::Pending,
                    error: None,
                })
                .collect(),
            error: None,
            started_at: now_ms(),
            job: Some(SendJob {
                target: device,
                remote_session_id: None,
                cancel: cancel.clone(),
                paths: planned
                    .iter()
                    .map(|planned| (planned.id.clone(), PathBuf::from(&planned.file.path)))
                    .collect(),
            }),
            saved_dir: None,
        };
        self.sessions.lock().push(session);
        self.emit_state();
        // Not `tokio::spawn`: the command that reaches this runs on the main
        // thread, which has no runtime context, and a panic there cannot unwind
        // out of the webview's callback — it aborts the whole application.
        tauri::async_runtime::spawn(run_send(
            self.clone(),
            session_id.clone(),
            planned,
            pin,
            cancel,
        ));
        Ok(session_id)
    }

    /// Cancels a session, telling the peer when it already knows the session.
    pub async fn cancel_session(self: &Arc<Self>, id: &str) {
        let known = {
            let sessions = self.sessions.lock();
            sessions
                .iter()
                .find(|session| session.id == id)
                .map(|session| {
                    (
                        session.direction,
                        session.job.as_ref().map(|job| {
                            (
                                job.target.clone(),
                                job.remote_session_id.clone(),
                                job.cancel.clone(),
                            )
                        }),
                    )
                })
        };
        let Some((direction, job)) = known else {
            return;
        };
        if direction == Direction::Receive {
            // Frees the single session slot and drops the sender's connection.
            let server = self.server.lock().clone();
            if let Some(server) = server {
                server.cancel_session(id).await;
            }
        }
        if let Some((target, remote_session_id, cancel)) = job {
            cancel.cancel();
            if direction == Direction::Send
                && let Some(remote_session_id) = remote_session_id
                && let Ok(client) = build_client(self, &target, None)
            {
                // The peer must not keep waiting for files that never come.
                client.cancel(&remote_session_id).await;
            }
        }
        self.finish_session(id, SessionStatus::Cancelled, None);
    }

    /// Sends one file again, as a session of its own.
    pub fn retry_file(self: &Arc<Self>, session_id: &str, file_id: &str) -> Result<String> {
        let (file, target) = {
            let sessions = self.sessions.lock();
            let session = sessions
                .iter()
                .find(|session| session.id == session_id)
                .with_context(|| format!("unknown session {session_id}"))?;
            let file = session
                .files
                .iter()
                .find(|file| file.id == file_id)
                .with_context(|| format!("unknown file {file_id}"))?;
            let path = session
                .job
                .as_ref()
                .and_then(|job| job.paths.get(file_id).cloned())
                .with_context(|| "the session no longer has the file's path")?;
            let target = session
                .job
                .as_ref()
                .map(|job| job.target.fingerprint.clone())
                .with_context(|| "the session no longer has a target")?;
            (
                SendFile {
                    path: path.display().to_string(),
                    name: file.name.clone(),
                    size: file.size,
                    mime: file.mime.clone(),
                    sha256: None,
                    modified: None,
                    accessed: None,
                },
                target,
            )
        };
        self.start_send(&target, vec![file], None)
    }

    // -----------------------------------------------------------------------
    // Receiving
    // -----------------------------------------------------------------------

    /// Answers the pending incoming request.
    pub async fn respond_to_upload(
        self: &Arc<Self>,
        session_id: &str,
        accept: bool,
        file_ids: Option<Vec<String>>,
    ) -> Result<()> {
        let incoming = self.incoming.lock().take();
        let Some(mut incoming) = incoming else {
            anyhow::bail!("there is no incoming transfer to answer");
        };
        anyhow::ensure!(
            incoming.request.session_id == session_id,
            "that request is no longer pending"
        );
        let Some(decision) = incoming.decision.take() else {
            anyhow::bail!("that request was already answered");
        };

        if !accept {
            let _ = decision.send(UploadDecision::Decline);
            self.record_incoming_history(&incoming, SessionStatus::Declined);
            self.emit_state();
            return Ok(());
        }

        let ids = match file_ids {
            Some(ids) if !ids.is_empty() => ids,
            Some(_) => anyhow::bail!("accepting needs at least one file"),
            None => incoming
                .request
                .files
                .iter()
                .map(|file| file.id.clone())
                .collect(),
        };
        let settings = self.settings.lock().clone();
        let accepted: Vec<OfferedFile> = incoming
            .request
            .files
            .iter()
            .filter(|file| ids.contains(&file.id))
            .cloned()
            .collect();

        // The session exists as soon as the peer is told, so progress has
        // somewhere to land.
        self.sessions.lock().push(Session {
            id: session_id.to_string(),
            direction: Direction::Receive,
            peer: incoming.request.peer.clone(),
            status: SessionStatus::Active,
            files: accepted
                .iter()
                .map(|file| TransferFile {
                    id: file.id.clone(),
                    name: file.name.clone(),
                    size: file.size,
                    mime: file.mime.clone(),
                    transferred: 0,
                    status: FileStatus::Pending,
                    error: None,
                })
                .collect(),
            error: None,
            started_at: now_ms(),
            job: None,
            saved_dir: Some(settings.download_dir.clone()),
        });

        let accepted_ids: Vec<String> = accepted.iter().map(|file| file.id.clone()).collect();
        if decision
            .send(UploadDecision::Accept {
                file_ids: accepted_ids,
                destination: settings.download_dir,
            })
            .is_err()
        {
            // The sender is gone; the session would never progress.
            self.finish_session(
                session_id,
                SessionStatus::Failed,
                Some("the sender disconnected".to_string()),
            );
            return Ok(());
        }
        self.emit_state();
        Ok(())
    }

    fn record_incoming_history(&self, incoming: &Incoming, status: SessionStatus) {
        let entry = HistoryEntry {
            id: incoming.request.session_id.clone(),
            direction: Direction::Receive,
            peer_alias: incoming.request.peer.alias.clone(),
            peer_fingerprint: incoming.request.peer.fingerprint.clone(),
            file_count: incoming.request.files.len(),
            total_size: incoming.request.total_size,
            status,
            at: now_ms(),
            saved_dir: None,
        };
        self.history.lock().insert(0, entry);
        self.history.lock().truncate(MAX_HISTORY);
    }

    /// Removes a finished session from the list.
    pub fn dismiss_transfer(&self, id: &str) -> Result<()> {
        let mut sessions = self.sessions.lock();
        let index = sessions
            .iter()
            .position(|session| session.id == id)
            .with_context(|| format!("unknown session {id}"))?;
        anyhow::ensure!(
            sessions[index].status.is_terminal(),
            "cancel the transfer before dismissing it"
        );
        sessions.remove(index);
        drop(sessions);
        self.emit_state();
        Ok(())
    }

    pub fn clear_history(&self) {
        self.history.lock().clear();
        self.emit_state();
    }

    /// Stores new settings and restarts the server when it has to.
    pub fn update_settings(self: &Arc<Self>, settings: Settings) -> Snapshot {
        let settings = settings.normalized();
        let previous = self.settings.lock().clone();
        let restart = previous.needs_server_restart(&settings);
        if let Err(error) = settings.save(&self.settings_path()) {
            tracing::error!("cannot persist settings: {error:#}");
        }
        *self.settings.lock() = settings;
        if restart {
            let state = self.clone();
            tauri::async_runtime::spawn(async move { state.restart().await });
        } else {
            self.emit_state();
        }
        self.snapshot()
    }

    /// Handles one event from the transfer server.
    async fn handle_server_event(self: &Arc<Self>, event: ServerEvent) {
        match event {
            ServerEvent::Discovered { peer } => self.peer_registered(peer),
            ServerEvent::PrepareUpload {
                session_id,
                peer,
                files,
                decision,
            } => self.incoming_request(session_id, peer, files, decision),
            ServerEvent::UploadStarted {
                session_id,
                file_id,
            } => {
                self.with_file(&session_id, &file_id, |file| {
                    file.status = FileStatus::Active
                });
                self.with_session(&session_id, |session| {
                    session.status = SessionStatus::Active
                });
                self.emit_state();
            }
            ServerEvent::UploadProgress {
                session_id,
                file_id,
                transferred,
            } => {
                self.with_file(&session_id, &file_id, |file| file.transferred = transferred);
                self.emit_progress(&session_id, &file_id, transferred);
            }
            ServerEvent::UploadFinished {
                session_id,
                file_id,
                path,
            } => {
                self.with_file(&session_id, &file_id, |file| {
                    file.status = FileStatus::Done;
                    file.transferred = file.size;
                });
                tracing::info!("received {}", path.display());
                self.emit_state();
            }
            ServerEvent::UploadFailed {
                session_id,
                file_id,
                error,
            } => {
                self.with_file(&session_id, &file_id, |file| {
                    file.status = FileStatus::Failed;
                    file.error = Some(error.clone());
                });
                self.emit_state();
            }
            ServerEvent::SessionEnded {
                session_id,
                outcome,
            } => {
                let status = match outcome {
                    SessionOutcome::Completed => SessionStatus::Done,
                    SessionOutcome::Cancelled => SessionStatus::Cancelled,
                    SessionOutcome::TimedOut => SessionStatus::Failed,
                };
                let error = (outcome == SessionOutcome::TimedOut)
                    .then(|| "the sender stopped responding".to_string());
                if self
                    .with_session(&session_id, |session| session.status)
                    .is_some()
                {
                    self.finish_session(&session_id, status, error);
                }
            }
            ServerEvent::ListenerFailed { error } => {
                tracing::error!("the transfer server stopped listening: {error}");
                *self.server_status.lock() = ServerStatus {
                    running: false,
                    ..self.server_status.lock().clone()
                };
                self.emit_state();
            }
        }
    }

    /// A peer told us who it is; remember it as a device.
    fn peer_registered(&self, peer: PeerIdentity) {
        let device = DiscoveredDevice {
            fingerprint: peer.fingerprint(),
            alias: peer.info.alias.clone(),
            version: peer.info.version.clone(),
            device_model: peer.info.device_model.clone(),
            device_type: peer.info.device_type,
            download: peer.info.download,
            protocol: peer.info.protocol,
            host: peer.address.to_string(),
            port: peer.info.port,
            last_seen: SystemTime::now(),
        };
        if let Some(discovery) = self.discovery.lock().as_ref() {
            discovery.add(device);
        }
    }

    /// Someone wants to send us files.
    fn incoming_request(
        self: &Arc<Self>,
        session_id: String,
        peer: PeerIdentity,
        files: Vec<FileDto>,
        decision: oneshot::Sender<UploadDecision>,
    ) {
        let settings = self.settings.lock().clone();
        let request = IncomingRequest {
            session_id: session_id.clone(),
            peer: TransferPeer {
                alias: peer.info.alias.clone(),
                fingerprint: peer.fingerprint(),
                device_model: peer.info.device_model.clone(),
                device_type: peer.info.device_type.unwrap_or_default(),
            },
            files: files
                .iter()
                .map(|file| OfferedFile {
                    id: file.id.clone(),
                    name: file.file_name.clone(),
                    size: file.size,
                    mime: file.file_type.clone(),
                })
                .collect(),
            total_size: files.iter().map(|file| file.size).sum(),
            // The sender had to know the PIN, which is worth showing.
            pin_protected: settings.pin.is_some(),
        };

        if settings.auto_accept {
            let ids = files.iter().map(|file| file.id.clone()).collect();
            self.sessions.lock().push(Session {
                id: session_id,
                direction: Direction::Receive,
                peer: request.peer.clone(),
                status: SessionStatus::Active,
                files: files
                    .iter()
                    .map(|file| TransferFile {
                        id: file.id.clone(),
                        name: file.file_name.clone(),
                        size: file.size,
                        mime: file.file_type.clone(),
                        transferred: 0,
                        status: FileStatus::Pending,
                        error: None,
                    })
                    .collect(),
                error: None,
                started_at: now_ms(),
                job: None,
                saved_dir: Some(settings.download_dir.clone()),
            });
            let _ = decision.send(UploadDecision::Accept {
                file_ids: ids,
                destination: settings.download_dir,
            });
            self.emit_state();
            return;
        }

        let had_previous = self
            .incoming
            .lock()
            .replace(Incoming {
                request,
                decision: Some(decision),
            })
            .is_some();
        if had_previous {
            tracing::warn!("replacing an unanswered incoming request");
        }
        self.emit_state();
    }
}

/// Payload of the `progress` event.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ProgressPayload {
    session_id: String,
    file_id: String,
    transferred: u64,
}

/// Runs one outgoing transfer to its end.
async fn run_send(
    state: Arc<AppState>,
    session_id: String,
    files: Vec<PlannedFile>,
    pin: Option<String>,
    cancel: CancellationToken,
) {
    let Some(target) = state
        .with_session(&session_id, |session| {
            session.job.as_ref().map(|job| job.target.clone())
        })
        .flatten()
    else {
        return;
    };

    match send_files(
        &state,
        &session_id,
        &target,
        &files,
        pin.as_deref(),
        &cancel,
    )
    .await
    {
        Ok(()) => {
            let failed = state
                .with_session(&session_id, |session| {
                    session
                        .files
                        .iter()
                        .filter(|file| file.status == FileStatus::Failed)
                        .count()
                })
                .unwrap_or(0);
            let status = if failed > 0 {
                SessionStatus::Failed
            } else {
                SessionStatus::Done
            };
            let error = (failed > 0).then(|| format!("{failed} file(s) failed"));
            state.finish_session(&session_id, status, error);
        }
        Err(SendFailure::Cancelled) => {
            state.finish_session(&session_id, SessionStatus::Cancelled, None);
        }
        Err(SendFailure::Status { status, message }) => {
            let (state_status, error) = match status {
                401 => (SessionStatus::PinRequired, None),
                403 => (SessionStatus::Declined, None),
                409 => (SessionStatus::Busy, None),
                429 => (
                    SessionStatus::Failed,
                    Some("too many attempts; wait a moment".to_string()),
                ),
                _ => (
                    SessionStatus::Failed,
                    Some(match message {
                        Some(message) => format!("[{status}] {message}"),
                        None => format!("peer answered {status}"),
                    }),
                ),
            };
            state.finish_session(&session_id, state_status, error);
        }
        Err(SendFailure::Other(error)) => {
            state.finish_session(
                &session_id,
                SessionStatus::Failed,
                Some(format!("{error:#}")),
            );
        }
    }
}

/// The failure cases the send flow reacts to.
enum SendFailure {
    Cancelled,
    Status {
        status: u16,
        message: Option<String>,
    },
    Other(anyhow::Error),
}

impl From<ClientError> for SendFailure {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Cancelled => SendFailure::Cancelled,
            ClientError::Status { status, message } => SendFailure::Status { status, message },
            ClientError::Other(error) => SendFailure::Other(error),
        }
    }
}

/// Hashes, offers and uploads the files of a session.
async fn send_files(
    state: &Arc<AppState>,
    session_id: &str,
    target: &DeviceOut,
    planned_files: &[PlannedFile],
    pin: Option<&str>,
    cancel: &CancellationToken,
) -> Result<(), SendFailure> {
    let settings = state.settings.lock().clone();
    let client = build_client(state, target, None).map_err(SendFailure::Other)?;

    // Checksums are computed before the request so the receiver can verify a
    // file the moment it has been received.
    let mut by_id = std::collections::BTreeMap::new();
    for planned in planned_files {
        if cancel.is_cancelled() {
            return Err(SendFailure::Cancelled);
        }
        let mut sha256 = planned.file.sha256.clone();
        if settings.create_checksums && sha256.is_none() {
            state.with_file(session_id, &planned.id, |entry| {
                entry.status = FileStatus::Hashing
            });
            state.emit_state();
            let path = PathBuf::from(&planned.file.path);
            match tokio::task::spawn_blocking(move || crabsend_core::crypto::sha256_hex_file(&path))
                .await
            {
                Ok(Ok(hash)) => sha256 = Some(hash),
                // A file that cannot be hashed can still be sent.
                Ok(Err(error)) => tracing::warn!("cannot hash {}: {error:#}", planned.file.path),
                Err(error) => tracing::warn!("hashing task failed: {error}"),
            }
            state.with_file(session_id, &planned.id, |entry| {
                entry.status = FileStatus::Pending
            });
            state.emit_state();
        }
        by_id.insert(
            planned.id.clone(),
            FileDto {
                id: planned.id.clone(),
                file_name: planned.file.name.clone(),
                size: planned.file.size,
                file_type: planned.file.mime.clone(),
                sha256,
                preview: None,
                metadata: metadata(&planned.file),
            },
        );
    }

    let request = PrepareUploadRequest {
        info: local_info(state),
        files: by_id,
    };
    state.with_session(session_id, |session| {
        session.status = SessionStatus::Waiting
    });
    state.emit_state();

    let outcome = client
        .prepare_upload(&request, pin, cancel)
        .await
        .map_err(SendFailure::from)?;
    let accepted = match outcome {
        PrepareOutcome::NothingToTransfer => {
            state.finish_session(session_id, SessionStatus::Done, None);
            return Ok(());
        }
        PrepareOutcome::Accepted {
            session_id: remote,
            files,
        } => (remote, files),
    };
    let (remote_session_id, tokens) = accepted;
    state.with_session(session_id, |session| {
        session.status = SessionStatus::Active;
        if let Some(job) = &mut session.job {
            job.remote_session_id = Some(remote_session_id.clone());
        }
        // Files the receiver did not ask for are not sent at all.
        for file in session.files.iter_mut() {
            if !tokens.contains_key(&file.id) {
                file.status = FileStatus::Skipped;
            }
        }
    });
    state.emit_state();

    let jobs: Vec<(String, PathBuf, String)> = planned_files
        .iter()
        .filter_map(|planned| {
            let token = tokens.get(&planned.id)?.clone();
            Some((planned.id.clone(), PathBuf::from(&planned.file.path), token))
        })
        .collect();

    let results: Vec<(String, std::result::Result<(), ClientError>)> =
        futures_util::stream::iter(jobs.into_iter().map(|(file_id, path, token)| {
            let client = &client;
            let remote_session_id = remote_session_id.clone();
            let state = state.clone();
            let session_id = session_id.to_string();
            let cancel = cancel.clone();
            async move {
                let result = upload_with_retry(
                    client,
                    &state,
                    &session_id,
                    &remote_session_id,
                    &file_id,
                    &token,
                    &path,
                    &cancel,
                )
                .await;
                (file_id, result)
            }
        }))
        .buffer_unordered(UPLOAD_CONCURRENCY)
        .collect()
        .await;

    let mut failure: Option<SendFailure> = None;
    for (file_id, result) in results {
        if let Err(error) = result {
            state.with_file(session_id, &file_id, |file| {
                file.status = FileStatus::Failed;
                file.error = Some(match &error {
                    ClientError::Status { status, message } => match message {
                        Some(message) => format!("[{status}] {message}"),
                        None => format!("peer answered {status}"),
                    },
                    other => format!("{other}"),
                });
            });
            if failure.is_none() {
                failure = Some(error.into());
            }
        }
    }
    state.emit_state();
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Uploads one file, retrying the checksum-mismatch case like the reference
/// implementation does.
#[allow(clippy::too_many_arguments)]
async fn upload_with_retry(
    client: &HttpClient,
    state: &Arc<AppState>,
    session_id: &str,
    remote_session_id: &str,
    file_id: &str,
    token: &str,
    path: &Path,
    cancel: &CancellationToken,
) -> std::result::Result<(), ClientError> {
    state.with_file(session_id, file_id, |file| {
        file.status = FileStatus::Active;
        file.transferred = 0;
    });
    state.emit_state();

    for attempt in 1..=MAX_UPLOAD_ATTEMPTS {
        let progress_state = state.clone();
        let progress_session = session_id.to_string();
        let progress_file = file_id.to_string();
        let result = client
            .upload(
                remote_session_id,
                file_id,
                token,
                path,
                move |transferred| {
                    progress_state.with_file(&progress_session, &progress_file, |file| {
                        file.transferred = transferred;
                    });
                    progress_state.emit_progress(&progress_session, &progress_file, transferred);
                },
                cancel,
            )
            .await;
        match result {
            Ok(()) => {
                state.with_file(session_id, file_id, |file| {
                    file.status = FileStatus::Done;
                    file.transferred = file.size;
                });
                state.emit_state();
                return Ok(());
            }
            Err(ClientError::Status { status: 422, .. }) if attempt < MAX_UPLOAD_ATTEMPTS => {
                tracing::warn!("{file_id}: checksum mismatch, retrying (attempt {attempt})");
            }
            Err(error) => return Err(error),
        }
    }
    Err(ClientError::Status {
        status: 422,
        message: Some("Checksum mismatch".to_string()),
    })
}

/// Builds a client pinned to the target's fingerprint.
fn build_client(
    state: &Arc<AppState>,
    target: &DeviceOut,
    timeout: Option<std::time::Duration>,
) -> Result<HttpClient> {
    let http_target = HttpTarget::new(target.protocol, target.host.clone(), target.port);
    // Over HTTPS the fingerprint proven by the handshake is the peer's
    // identity; over HTTP there is nothing to pin to.
    let pin = match target.protocol {
        ProtocolType::Https => Some(target.fingerprint.as_str()),
        ProtocolType::Http => None,
    };
    HttpClient::new(&state.identity, &http_target, pin, timeout)
}

/// What this device announces to peers.
pub fn local_info(state: &AppState) -> RegisterDto {
    let settings = state.settings.lock().clone();
    let status = state.server_status.lock().clone();
    RegisterDto {
        alias: settings.alias,
        version: crabsend_core::model::PROTOCOL_VERSION.to_string(),
        device_model: settings.device_model,
        device_type: Some(settings.device_type),
        fingerprint: state.identity.fingerprint.clone(),
        port: status.port,
        protocol: status.protocol,
        download: false,
    }
}

/// File timestamps in the RFC 3339 form the protocol uses.
fn metadata(file: &SendFile) -> Option<FileMetadata> {
    if file.modified.is_none() && file.accessed.is_none() {
        return None;
    }
    Some(FileMetadata {
        modified: file.modified.clone(),
        accessed: file.accessed.clone(),
    })
}

fn device_out(device: DiscoveredDevice) -> DeviceOut {
    DeviceOut {
        fingerprint: device.fingerprint,
        alias: device.alias,
        device_model: device.device_model,
        device_type: device.device_type.unwrap_or_default(),
        protocol: device.protocol,
        host: device.host,
        port: device.port,
        download: device.download,
        last_seen: device
            .last_seen
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or_default(),
        paired: false,
    }
}

/// Carries files that were received before the download directory moved.
///
/// Best effort: a file that cannot be moved stays where it is rather than
/// stopping the application from starting.
fn move_received_files(from: &Path, to: &Path) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    if std::fs::create_dir_all(to).is_err() {
        return;
    }
    for entry in entries.flatten() {
        let source = entry.path();
        let target = to.join(entry.file_name());
        // A rename is impossible between the two directories on a phone: they
        // are separate mounts of its storage layer, which answers with
        // `Cross-device link`.
        if std::fs::rename(&source, &target).is_ok() {
            continue;
        }
        if let Err(error) = copy_then_remove(&source, &target) {
            tracing::warn!(
                "cannot move {} to {}: {error}",
                source.display(),
                target.display()
            );
        }
    }
}

/// Copies a file and then drops the original.
///
/// The copy is done by hand rather than with [`std::fs::copy`]: that uses
/// `copy_file_range`, which the storage layer of a phone also refuses across
/// mounts, and it does not fall back.
fn copy_then_remove(source: &Path, target: &Path) -> std::io::Result<()> {
    let mut from = std::fs::File::open(source)?;
    let mut to = std::fs::File::create(target)?;
    std::io::copy(&mut from, &mut to)?;
    drop(to);
    std::fs::remove_file(source)
}

/// Where received files are written on this platform.
///
/// On the desktop that is the usual downloads directory. A phone has two
/// directories of its own, and the interface must pick the one its owner can
/// find: the media directory is visible to file managers, the files directory
/// next to it is not.
fn platform_download_dir(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().download_dir().ok()?;
    if let Some(shared) = crate::settings::android_media_dir(&dir) {
        return Some(shared);
    }
    Some(dir.join("Crabsend"))
}

/// A stored pairing, as the device list shows it. The address is the one the
/// pairing code carried; a scan that reaches the peer replaces it.
fn paired_device(peer: &PairedPeer) -> DeviceOut {
    DeviceOut {
        fingerprint: peer.fingerprint.clone(),
        alias: peer.alias.clone(),
        device_model: peer.device_model.clone(),
        device_type: peer.device_type,
        protocol: peer.protocol,
        host: peer.host.clone(),
        port: peer.port,
        // A paired peer is dialled directly; it never offered the browser
        // download API this flag advertises.
        download: false,
        last_seen: peer.last_seen,
        paired: true,
    }
}

/// Splits what the user typed into an address and a port.
///
/// `host`, `host:port`, `[::1]` and `[::1]:port` are all accepted; a bare IPv6
/// literal is recognised by its colons and keeps the protocol default port.
fn split_host_port(text: &str) -> Result<(String, u16)> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "enter a host name or address");
    if let Some(rest) = text.strip_prefix('[') {
        let (address, tail) = rest
            .split_once(']')
            .context("the address is missing its closing bracket")?;
        let port = match tail.trim().strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None => crabsend_core::model::DEFAULT_PORT,
        };
        return Ok((address.trim().to_string(), port));
    }
    if text.matches(':').count() > 1 {
        return Ok((text.to_string(), crabsend_core::model::DEFAULT_PORT));
    }
    match text.split_once(':') {
        Some((address, port)) => Ok((address.trim().to_string(), parse_port(port)?)),
        None => Ok((text.to_string(), crabsend_core::model::DEFAULT_PORT)),
    }
}

fn parse_port(text: &str) -> Result<u16> {
    let port: u16 = text
        .trim()
        .parse()
        .context("the port must be a number between 1 and 65535")?;
    anyhow::ensure!(port != 0, "the port must be between 1 and 65535");
    Ok(port)
}

/// Reads the paired devices, treating a missing or unreadable file as "none"
/// so a broken file never blocks startup.
fn load_peers(path: &Path) -> Vec<PairedPeer> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!("cannot read {}: {error}", path.display());
            return Vec::new();
        }
    };
    match serde_json::from_str(&contents) {
        Ok(peers) => peers,
        Err(error) => {
            tracing::warn!(
                "ignoring unreadable paired devices at {}: {error}",
                path.display()
            );
            Vec::new()
        }
    }
}

fn write_peers(path: &Path, peers: &[PairedPeer]) -> Result<()> {
    let json = serde_json::to_string_pretty(peers)?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Reads the `progress` receiver of the server and applies every event.
async fn pump_server_events(
    state: Arc<AppState>,
    mut events: mpsc::UnboundedReceiver<ServerEvent>,
) {
    while let Some(event) = events.recv().await {
        state.handle_server_event(event).await;
    }
}

/// Loads the device certificate, generating and storing one on first start.
fn load_or_create_identity(config_dir: &Path) -> Result<Identity> {
    let path = config_dir.join("identity.pem");
    if let Ok(contents) = std::fs::read_to_string(&path) {
        let (cert, key) = split_pem(&contents);
        if let Some(identity) = cert
            .zip(key)
            .and_then(|(cert, key)| Identity::from_pem(cert, key).ok())
        {
            return Ok(identity);
        }
        tracing::warn!(
            "{} is unreadable; generating a new identity",
            path.display()
        );
    }

    let identity = Identity::generate().context("generating the device certificate")?;
    let contents = format!("{}{}", identity.cert_pem, identity.key_pem);
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The file holds the private key.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(identity)
}

/// Splits a stored identity file back into its certificate and key.
fn split_pem(contents: &str) -> (Option<String>, Option<String>) {
    let certificate_end = contents.find("-----END CERTIFICATE-----");
    let Some(end) = certificate_end else {
        return (None, None);
    };
    let cert = contents[..end + "-----END CERTIFICATE-----".len()].to_string();
    let key = contents[end + "-----END CERTIFICATE-----".len()..]
        .trim()
        .to_string();
    let key = (!key.is_empty()).then_some(key);
    (Some(cert), key)
}

/// Milliseconds since the epoch, for display ordering.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_identity_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_identity(dir.path()).unwrap();
        let second = load_or_create_identity(dir.path()).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn a_corrupt_identity_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_identity(dir.path()).unwrap();
        std::fs::write(dir.path().join("identity.pem"), "garbage").unwrap();
        let second = load_or_create_identity(dir.path()).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn moving_received_files_carries_them_over() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("hidden");
        let to = root.path().join("shared");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("note.txt"), b"hello").unwrap();

        move_received_files(&from, &to);

        assert_eq!(std::fs::read(to.join("note.txt")).unwrap(), b"hello");
        assert!(!from.join("note.txt").exists());
        // A directory that was never there is not an error.
        move_received_files(&root.path().join("nowhere"), &to);
    }

    #[test]
    fn pem_split_keeps_the_certificate_and_key_apart() {
        let identity = Identity::generate().unwrap();
        let contents = format!("{}{}", identity.cert_pem, identity.key_pem);
        let (cert, key) = split_pem(&contents);
        let rebuilt = Identity::from_pem(cert.unwrap(), key.unwrap()).unwrap();
        assert_eq!(rebuilt.fingerprint, identity.fingerprint);
    }

    #[test]
    fn a_typed_host_may_carry_its_own_port() {
        let default = crabsend_core::model::DEFAULT_PORT;
        for (typed, address, port) in [
            ("192.168.1.5", "192.168.1.5", default),
            (" 192.168.1.5:53400 ", "192.168.1.5", 53400),
            ("nas.local", "nas.local", default),
            ("fe80::1", "fe80::1", default),
            ("[fe80::1]:53400", "fe80::1", 53400),
            ("[fe80::1]", "fe80::1", default),
        ] {
            let (host, found) = split_host_port(typed).unwrap();
            assert_eq!((host.as_str(), found), (address, port), "for {typed}");
        }
    }

    #[test]
    fn an_unusable_host_is_refused() {
        assert!(split_host_port("").is_err());
        assert!(split_host_port("   ").is_err());
        assert!(split_host_port("host:0").is_err());
        assert!(split_host_port("host:70000").is_err());
        assert!(split_host_port("host:http").is_err());
        assert!(split_host_port("[fe80::1").is_err());
    }
}
