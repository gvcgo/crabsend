//! The receiving side: an HTTP(S) server implementing the LocalSend v2 API.
//!
//! The server owns exactly one upload session at a time — the protocol has a
//! single session slot, and a second `prepare-upload` while one is pending is
//! answered with `409` so two senders cannot interleave their files.

use parking_lot::Mutex as SyncMutex;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bytes::Bytes;
use futures_util::StreamExt;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::Service;

use crate::crypto::Identity;
use crate::crypto::fingerprint_from_cert_der;
use crate::crypto::random_id;
use crate::fs_util;
use crate::model::DeviceInfoDto;
use crate::model::DeviceType;
use crate::model::ErrorResponse;
use crate::model::FileDto;
use crate::model::PROTOCOL_VERSION;
use crate::model::PrepareUploadRequest;
use crate::model::PrepareUploadResponse;
use crate::model::ProtocolType;
use crate::model::RegisterDto;
use crate::tls;

/// How long a sender waits for the user to decide on an incoming request
/// before the request is declined and the session slot is released.
const DECISION_TIMEOUT: Duration = Duration::from_secs(120);

/// How long an accepted session may go without receiving any byte before it is
/// abandoned: a sender that walks away must not block the port for good.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Largest JSON body accepted on the metadata endpoints.
const MAX_JSON_BODY: usize = 64 * 1024 * 1024;

/// Minimum interval between progress events for one file.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Write buffer size for received files.
const WRITE_BUFFER_SIZE: usize = 512 * 1024;

/// How many times a sender may retry one file after a checksum mismatch.
const MAX_UPLOAD_ATTEMPTS: u8 = 3;

/// Number of failed PIN attempts allowed per address before requests are
/// rejected outright.
const MAX_PIN_ATTEMPTS: u32 = 3;

/// Something the server needs the application to know about.
pub enum ServerEvent {
    /// A peer announced itself.
    Discovered { peer: PeerIdentity },
    /// A peer wants to send files. The application answers on `decision`.
    PrepareUpload {
        session_id: String,
        peer: PeerIdentity,
        files: Vec<FileDto>,
        decision: oneshot::Sender<UploadDecision>,
    },
    /// A file upload has started.
    UploadStarted { session_id: String, file_id: String },
    /// Bytes received so far for one file.
    UploadProgress {
        session_id: String,
        file_id: String,
        transferred: u64,
    },
    /// A file was received and stored.
    UploadFinished {
        session_id: String,
        file_id: String,
        path: PathBuf,
    },
    /// A file could not be stored.
    UploadFailed {
        session_id: String,
        file_id: String,
        error: String,
    },
    /// The session is over.
    SessionEnded {
        session_id: String,
        outcome: SessionOutcome,
    },
    /// The listening socket failed; the application should restart the server.
    ListenerFailed { error: String },
}

/// How a session ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOutcome {
    /// Every offered file was accounted for.
    Completed,
    /// The sender cancelled.
    Cancelled,
    /// The sender went away and the session was abandoned.
    TimedOut,
}

/// What the application decided about an incoming request.
pub enum UploadDecision {
    /// Receive these file ids into this directory.
    Accept {
        file_ids: Vec<String>,
        destination: PathBuf,
    },
    /// Refuse the whole request.
    Decline,
}

/// The application-facing view of a peer.
#[derive(Clone, Debug)]
pub struct PeerIdentity {
    /// What the peer claimed about itself.
    pub info: RegisterDto,
    /// Where the request came from.
    pub address: IpAddr,
    /// Fingerprint proven by the TLS handshake, when the peer used HTTPS.
    pub cert_fingerprint: Option<String>,
}

impl PeerIdentity {
    /// The fingerprint to key this peer by: proven over TLS, claimed over HTTP.
    pub fn fingerprint(&self) -> String {
        match &self.cert_fingerprint {
            Some(fingerprint) => fingerprint.clone(),
            None => self.info.fingerprint.to_ascii_uppercase(),
        }
    }
}

/// Everything the server needs to run.
pub struct ServerConfig {
    /// TCP port; `0` binds an ephemeral one (used by tests).
    pub port: u16,
    /// The device certificate. `None` serves plain HTTP.
    pub identity: Option<Identity>,
    /// Fingerprint announced over plain HTTP, where there is no certificate.
    pub http_fingerprint: String,
    pub alias: String,
    pub device_model: Option<String>,
    pub device_type: Option<DeviceType>,
    /// Optional PIN required from senders.
    pub pin: Option<String>,
    /// Verify the `sha256` a sender provides and answer `422` on mismatch.
    pub verify_checksums: bool,
    /// Whether the browser download API is announced as available.
    pub download_enabled: bool,
    pub events: mpsc::UnboundedSender<ServerEvent>,
}

/// A running server.
pub struct Server {
    state: Arc<Shared>,
    port: u16,
    protocol: ProtocolType,
    shutdown: CancellationToken,
    /// Taken when the server is stopped; the handle stays behind so a `Server`
    /// can be shared and still be shut down exactly once.
    task: SyncMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Server {
    /// Binds the port and starts accepting connections.
    ///
    /// Binding the IPv4 socket is required; the IPv6 socket is best effort so a
    /// host without usable IPv6 still serves.
    pub async fn start(config: ServerConfig) -> Result<Self> {
        tls::install_crypto_provider();

        let protocol = if config.identity.is_some() {
            ProtocolType::Https
        } else {
            ProtocolType::Http
        };
        let acceptor = match &config.identity {
            Some(identity) => Some(TlsAcceptor::from(Arc::new(tls::server_config(
                identity,
                // Every LocalSend peer presents a certificate; requiring one
                // keeps the fingerprint a peer announces provable.
                true,
            )?))),
            None => None,
        };
        let fingerprint = match &config.identity {
            Some(identity) => identity.fingerprint.clone(),
            None => config.http_fingerprint.clone(),
        };

        let ipv4 = TcpListener::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), config.port))
            .await
            .with_context(|| format!("binding TCP port {}", config.port))?;
        let port = ipv4.local_addr()?.port();
        let ipv6 = match bind_ipv6_only(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port)) {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing::warn!("IPv6 listener unavailable on port {port}: {error}");
                None
            }
        };

        let state = Arc::new(Shared {
            alias: config.alias,
            device_model: config.device_model,
            device_type: config.device_type,
            fingerprint,
            download_enabled: config.download_enabled,
            pin: config.pin,
            verify_checksums: config.verify_checksums,
            events: config.events,
            session: Mutex::new(None),
            pin_attempts: Mutex::new(HashMap::new()),
        });

        let shutdown = CancellationToken::new();
        let task = tokio::spawn(serve(
            state.clone(),
            router(state.clone()),
            ipv4,
            ipv6,
            acceptor,
            shutdown.clone(),
        ));

        Ok(Self {
            state,
            port,
            protocol,
            shutdown,
            task: SyncMutex::new(Some(task)),
        })
    }

    /// The port actually bound.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The transport peers must use to reach this server.
    pub fn protocol(&self) -> ProtocolType {
        self.protocol
    }

    /// Stops accepting connections and closes the listeners.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        // The guard is released before the await.
        let task = self.task.lock().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Drops a session whose peer is still waiting, e.g. because the user quit
    /// the application.
    pub async fn cancel_session(&self, session_id: &str) {
        let mut slot = self.state.session.lock().await;
        let ours = matches!(
            slot.as_ref(),
            Some(session) if session.id() == session_id
        );
        if ours {
            *slot = None;
            drop(slot);
            let _ = self.state.events.send(ServerEvent::SessionEnded {
                session_id: session_id.to_string(),
                outcome: SessionOutcome::Cancelled,
            });
        }
    }
}

/// State shared by every connection and handler.
struct Shared {
    alias: String,
    device_model: Option<String>,
    device_type: Option<DeviceType>,
    fingerprint: String,
    download_enabled: bool,
    pin: Option<String>,
    verify_checksums: bool,
    events: mpsc::UnboundedSender<ServerEvent>,
    session: Mutex<Option<Session>>,
    /// Failed PIN attempts per peer address.
    pin_attempts: Mutex<HashMap<IpAddr, u32>>,
}

impl Shared {
    fn device_info(&self) -> DeviceInfoDto {
        DeviceInfoDto {
            alias: self.alias.clone(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: self.device_model.clone(),
            device_type: self.device_type,
            fingerprint: self.fingerprint.clone(),
            download: self.download_enabled,
        }
    }
}

/// The API routes.
fn router(state: Arc<Shared>) -> Router {
    Router::new()
        .route("/api/localsend/v2/register", post(register))
        // Devices running v1.17 and earlier probe unknown peers here.
        .route("/api/localsend/v1/info", get(info))
        .route("/api/localsend/v2/info", get(info))
        .route("/api/localsend/v2/prepare-upload", post(prepare_upload))
        .route("/api/localsend/v2/upload", post(upload))
        .route("/api/localsend/v2/cancel", post(cancel))
        .with_state(state)
}

/// One upload session; the protocol allows only one at a time.
enum Session {
    Pending(PendingSession),
    Active(ActiveSession),
}

impl Session {
    fn id(&self) -> &str {
        match self {
            Session::Pending(pending) => &pending.id,
            Session::Active(active) => &active.id,
        }
    }
}

struct PendingSession {
    id: String,
    sender_ip: IpAddr,
    cancel: CancellationToken,
}

struct ActiveSession {
    id: String,
    sender_ip: IpAddr,
    destination: PathBuf,
    files: HashMap<String, SessionFile>,
    last_activity: Instant,
}

struct SessionFile {
    dto: FileDto,
    token: String,
    status: FileStatus,
    attempts: u8,
}

/// A file is `Pending` until it has been received; a checksum mismatch within
/// the retry budget puts it back to `Pending`.
#[derive(Clone, Copy, Eq, PartialEq)]
enum FileStatus {
    Pending,
    Finished,
    Failed,
}

/// Peer information extracted from the connection.
#[derive(Clone, Debug)]
struct PeerInfo {
    ip: IpAddr,
    cert_fingerprint: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PinQuery {
    pin: Option<String>,
    session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadQuery {
    session_id: Option<String>,
    file_id: Option<String>,
    token: Option<String>,
}

/// Announces a device to us; the response carries our own identity.
async fn register(
    State(state): State<Arc<Shared>>,
    Extension(peer): Extension<PeerInfo>,
    body: Bytes,
) -> Response {
    let payload: RegisterDto = match parse_json(&body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };

    // On HTTPS the certificate is the peer's identity, so a claim that does not
    // match it is not a usable registration.
    let proven = match &peer.cert_fingerprint {
        Some(fingerprint) => fingerprint.eq_ignore_ascii_case(payload.fingerprint.trim()),
        // Plain HTTP carries no proof at all; the claimed value is all there is.
        None => true,
    };
    if proven {
        let _ = state.events.send(ServerEvent::Discovered {
            peer: PeerIdentity {
                info: payload,
                address: peer.ip,
                cert_fingerprint: peer.cert_fingerprint.clone(),
            },
        });
    } else {
        tracing::warn!(
            "ignoring registration from {}: claimed fingerprint does not match its certificate",
            peer.ip
        );
    }

    json_response(StatusCode::OK, &state.device_info())
}

/// Reports this device's identity; used by peers for probes and diagnostics.
async fn info(State(state): State<Arc<Shared>>) -> Response {
    json_response(StatusCode::OK, &state.device_info())
}

/// Decides the file list of an incoming transfer.
async fn prepare_upload(
    State(state): State<Arc<Shared>>,
    Extension(peer): Extension<PeerInfo>,
    Query(query): Query<PinQuery>,
    body: Bytes,
) -> Response {
    if let Err(response) = check_pin(&state, &peer.ip, query.pin.as_deref()).await {
        return response;
    }
    let payload: PrepareUploadRequest = match parse_json(&body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    if payload.files.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "No files provided");
    }

    let session_id = random_id();
    let cancel = CancellationToken::new();
    let (decision_tx, decision_rx) = oneshot::channel();
    {
        let mut slot = state.session.lock().await;
        if slot.is_some() {
            return error_response(StatusCode::CONFLICT, "Blocked by another session");
        }
        *slot = Some(Session::Pending(PendingSession {
            id: session_id.clone(),
            sender_ip: peer.ip,
            cancel: cancel.clone(),
        }));
    }
    // Whatever happens next — a cancellation, the timeout, an early return —
    // the slot must not stay claimed.
    let guard = PendingGuard {
        state: state.clone(),
        session_id: session_id.clone(),
    };

    let files: Vec<FileDto> = payload.files.values().cloned().collect();
    if state
        .events
        .send(ServerEvent::PrepareUpload {
            session_id: session_id.clone(),
            peer: PeerIdentity {
                info: payload.info,
                address: peer.ip,
                cert_fingerprint: peer.cert_fingerprint.clone(),
            },
            files,
            decision: decision_tx,
        })
        .is_err()
    {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Status code: 500 Internal Server Error",
        );
    }

    let decision = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return error_response(StatusCode::FORBIDDEN, "Cancelled by sender");
        }
        _ = tokio::time::sleep(DECISION_TIMEOUT) => {
            tracing::info!("incoming request {session_id} not answered in time");
            return error_response(StatusCode::FORBIDDEN, "Rejected");
        }
        decision = decision_rx => decision,
    };

    let (file_ids, destination) = match decision {
        Ok(UploadDecision::Accept {
            file_ids,
            destination,
        }) => (file_ids, destination),
        // The decision channel is dropped when the application discards the
        // request, e.g. because the window closed mid-prompt.
        Ok(UploadDecision::Decline) | Err(_) => {
            return error_response(StatusCode::FORBIDDEN, "Rejected");
        }
    };

    let accepted: HashMap<String, SessionFile> = payload
        .files
        .into_iter()
        .filter(|(id, _)| file_ids.contains(id))
        .map(|(id, dto)| {
            let token = random_id();
            (
                id,
                SessionFile {
                    dto,
                    token,
                    status: FileStatus::Pending,
                    attempts: 0,
                },
            )
        })
        .collect();

    let mut slot = state.session.lock().await;
    if accepted.is_empty() {
        *slot = None;
        return empty_response(StatusCode::NO_CONTENT);
    }
    let responses: BTreeMap<String, String> = accepted
        .iter()
        .map(|(id, file)| (id.clone(), file.token.clone()))
        .collect();
    *slot = Some(Session::Active(ActiveSession {
        id: session_id.clone(),
        sender_ip: peer.ip,
        destination,
        files: accepted,
        last_activity: Instant::now(),
    }));
    drop(slot);
    // The session is active now; the guard must not clear it.
    guard.disarm();

    json_response(
        StatusCode::OK,
        &PrepareUploadResponse {
            session_id,
            files: responses,
        },
    )
}

/// Receives the content of one file.
async fn upload(
    State(state): State<Arc<Shared>>,
    Extension(peer): Extension<PeerInfo>,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> Response {
    let (Some(session_id), Some(file_id), Some(token)) =
        (query.session_id, query.file_id, query.token)
    else {
        return error_response(StatusCode::BAD_REQUEST, "Missing parameters");
    };

    let (dto, expected_sha256, destination) = {
        let mut slot = state.session.lock().await;
        match slot.as_mut() {
            Some(Session::Active(session))
                if session.id == session_id && session.sender_ip == peer.ip =>
            {
                session.last_activity = Instant::now();
                match session.files.get_mut(&file_id) {
                    Some(file) if file.token == token && file.status == FileStatus::Pending => {
                        file.attempts = file.attempts.saturating_add(1);
                        let expected = state
                            .verify_checksums
                            .then(|| file.dto.sha256.clone())
                            .flatten();
                        (file.dto.clone(), expected, session.destination.clone())
                    }
                    _ => {
                        return error_response(
                            StatusCode::FORBIDDEN,
                            "Invalid token or IP address",
                        );
                    }
                }
            }
            _ => {
                return error_response(StatusCode::FORBIDDEN, "Invalid token or IP address");
            }
        }
    };

    let path = match fs_util::resolve_destination(&destination, &dto.file_name) {
        Ok((path, _)) => path,
        Err(error) => {
            return settle_file(&state, &session_id, &file_id, Err(error.to_string())).await;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        return settle_file(
            &state,
            &session_id,
            &file_id,
            Err(format!("creating {}: {error}", parent.display())),
        )
        .await;
    }

    let _ = state.events.send(ServerEvent::UploadStarted {
        session_id: session_id.clone(),
        file_id: file_id.clone(),
    });

    let received = tokio::time::timeout(
        SESSION_IDLE_TIMEOUT,
        receive_body(
            body,
            &path,
            &dto,
            expected_sha256.as_deref(),
            &state.events,
            &session_id,
            &file_id,
        ),
    )
    .await
    .unwrap_or_else(|_| Err("the sender stopped sending data".to_string()));

    match received {
        Ok(()) => {
            let _ = state.events.send(ServerEvent::UploadFinished {
                session_id: session_id.clone(),
                file_id: file_id.clone(),
                path,
            });
            settle_file(&state, &session_id, &file_id, Ok(())).await
        }
        Err(error) => {
            // A partially written file is never useful to the user.
            let _ = tokio::fs::remove_file(&path).await;
            settle_file(&state, &session_id, &file_id, Err(error)).await
        }
    }
}

/// Marks a file done and answers the sender, releasing the session when no
/// file is left to receive.
async fn settle_file(
    state: &Arc<Shared>,
    session_id: &str,
    file_id: &str,
    result: Result<(), String>,
) -> Response {
    let mut slot = state.session.lock().await;
    let mut complete = false;
    if let Some(Session::Active(session)) = slot.as_mut()
        && session.id == session_id
    {
        if let Some(file) = session.files.get_mut(file_id) {
            match &result {
                Ok(()) => file.status = FileStatus::Finished,
                Err(error) => {
                    // Only a checksum mismatch is worth another attempt; the
                    // sender reuses the same token for it.
                    file.status = if is_checksum_error(error) && file.attempts < MAX_UPLOAD_ATTEMPTS
                    {
                        FileStatus::Pending
                    } else {
                        FileStatus::Failed
                    };
                }
            }
        }
        complete = session
            .files
            .values()
            .all(|file| file.status != FileStatus::Pending);
    }
    if complete {
        *slot = None;
    }
    drop(slot);

    match result {
        Ok(()) => {
            if complete {
                let _ = state.events.send(ServerEvent::SessionEnded {
                    session_id: session_id.to_string(),
                    outcome: SessionOutcome::Completed,
                });
            }
            empty_response(StatusCode::OK)
        }
        Err(error) => {
            tracing::warn!("receiving {file_id} failed: {error}");
            let _ = state.events.send(ServerEvent::UploadFailed {
                session_id: session_id.to_string(),
                file_id: file_id.to_string(),
                error: error.clone(),
            });
            if complete {
                let _ = state.events.send(ServerEvent::SessionEnded {
                    session_id: session_id.to_string(),
                    outcome: SessionOutcome::Completed,
                });
            }
            if is_checksum_error(&error) {
                error_response(StatusCode::UNPROCESSABLE_ENTITY, "Checksum mismatch")
            } else {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Status code: 500 Internal Server Error",
                )
            }
        }
    }
}

/// Ends a session because the sender cancelled or gave up.
async fn cancel(
    State(state): State<Arc<Shared>>,
    Extension(peer): Extension<PeerInfo>,
    Query(query): Query<PinQuery>,
) -> Response {
    let ended = {
        let mut slot = state.session.lock().await;
        let mut ended = None;
        match slot.as_ref() {
            // Before the response, a sender does not know the session id yet: a
            // cancel from the same address is the only signal that it gave up.
            Some(Session::Pending(pending))
                if pending.sender_ip == peer.ip
                    && query
                        .session_id
                        .as_deref()
                        .is_none_or(|id| id == pending.id) =>
            {
                pending.cancel.cancel();
                ended = Some(pending.id.clone());
                *slot = None;
            }
            Some(Session::Active(session))
                if session.sender_ip == peer.ip
                    && query.session_id.as_deref() == Some(session.id.as_str()) =>
            {
                ended = Some(session.id.clone());
                *slot = None;
            }
            _ => {}
        }
        ended
    };

    if let Some(session_id) = ended {
        let _ = state.events.send(ServerEvent::SessionEnded {
            session_id,
            outcome: SessionOutcome::Cancelled,
        });
    }
    empty_response(StatusCode::OK)
}

/// Releases the session slot unless the session became active.
struct PendingGuard {
    state: Arc<Shared>,
    session_id: String,
}

impl PendingGuard {
    /// Keeps the session that was just stored instead of clearing it on drop.
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.state.session.try_lock() {
            let ours = matches!(
                slot.as_ref(),
                Some(Session::Pending(pending)) if pending.id == self.session_id
            );
            if ours {
                *slot = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Receiving
// ---------------------------------------------------------------------------

/// Streams the request body into `path`, checking size and, when the sender
/// provided one, the SHA-256 checksum.
async fn receive_body(
    body: Body,
    path: &Path,
    dto: &FileDto,
    expected_sha256: Option<&str>,
    events: &mpsc::UnboundedSender<ServerEvent>,
    session_id: &str,
    file_id: &str,
) -> Result<(), String> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(|error| format!("creating {}: {error}", path.display()))?;
    let mut writer = tokio::io::BufWriter::with_capacity(WRITE_BUFFER_SIZE, file);
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut last_progress = Instant::now();
    let mut stream = body.into_data_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("reading the request body: {error}"))?;
        if expected_sha256.is_some() {
            hasher.update(&chunk);
        }
        written += chunk.len() as u64;
        if written > dto.size {
            return Err(format!(
                "expected {} bytes, received at least {written}",
                dto.size
            ));
        }
        writer
            .write_all(&chunk)
            .await
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
        if last_progress.elapsed() >= PROGRESS_INTERVAL || written == dto.size {
            last_progress = Instant::now();
            let _ = events.send(ServerEvent::UploadProgress {
                session_id: session_id.to_string(),
                file_id: file_id.to_string(),
                transferred: written,
            });
        }
    }

    if written != dto.size {
        return Err(format!("expected {} bytes, received {written}", dto.size));
    }
    if let Some(expected) = expected_sha256 {
        let actual = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "checksum mismatch: expected {expected}, got {actual}"
            ));
        }
    }

    writer
        .flush()
        .await
        .map_err(|error| format!("flushing {}: {error}", path.display()))?;
    apply_timestamps(writer.into_inner().into_std().await, dto);
    Ok(())
}

/// Restores the sender's timestamps on the received file; best effort.
fn apply_timestamps(file: std::fs::File, dto: &FileDto) {
    let Some(metadata) = &dto.metadata else {
        return;
    };
    let mut times = std::fs::FileTimes::new();
    let mut any = false;
    if let Some(modified) = parse_timestamp(metadata.modified.as_deref()) {
        times = times.set_modified(modified);
        any = true;
    }
    if let Some(accessed) = parse_timestamp(metadata.accessed.as_deref()) {
        times = times.set_accessed(accessed);
        any = true;
    }
    if any && let Err(error) = file.set_times(times) {
        tracing::debug!("could not restore the sender's timestamps: {error}");
    }
}

/// Parses an RFC 3339 timestamp, the format file metadata uses.
fn parse_timestamp(value: Option<&str>) -> Option<SystemTime> {
    let value = value?;
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(SystemTime::from)
}

/// Whether a receive failure was a checksum mismatch, which maps to `422`.
fn is_checksum_error(error: &str) -> bool {
    error.starts_with("checksum mismatch")
}

// ---------------------------------------------------------------------------
// PIN
// ---------------------------------------------------------------------------

/// Enforces the optional PIN on `prepare-upload`.
async fn check_pin(
    state: &Arc<Shared>,
    ip: &IpAddr,
    provided: Option<&str>,
) -> Result<(), Response> {
    let Some(required) = state.pin.as_deref().filter(|pin| !pin.is_empty()) else {
        return Ok(());
    };
    let mut attempts = state.pin_attempts.lock().await;
    let failures = attempts.get(ip).copied().unwrap_or(0);
    if failures >= MAX_PIN_ATTEMPTS {
        return Err(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests",
        ));
    }
    match provided {
        Some(pin) if pin == required => {
            attempts.remove(ip);
            Ok(())
        }
        Some(_) => {
            attempts.insert(*ip, failures + 1);
            Err(error_response(StatusCode::UNAUTHORIZED, "Invalid PIN"))
        }
        None => Err(error_response(StatusCode::UNAUTHORIZED, "PIN required")),
    }
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

fn parse_json<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    if body.len() > MAX_JSON_BODY {
        return Err(error_response(StatusCode::BAD_REQUEST, "Invalid JSON body"));
    }
    serde_json::from_slice(body)
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "Invalid JSON body"))
}

fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    (status, Json(value)).into_response()
}

fn empty_response(status: StatusCode) -> Response {
    status.into_response()
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(ErrorResponse::new(message))).into_response()
}

/// Binds an IPv6 socket that does not also capture IPv4 traffic, so the two
/// listeners can share the port.
fn bind_ipv6_only(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_only_v6(true)?;
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    TcpListener::from_std(std::net::TcpListener::from(socket))
}

/// Accepts connections on every bound listener until shutdown.
async fn serve(
    state: Arc<Shared>,
    router: Router,
    ipv4: TcpListener,
    ipv6: Option<TcpListener>,
    acceptor: Option<TlsAcceptor>,
    shutdown: CancellationToken,
) {
    let watchdog = tokio::spawn(watch_sessions(state.clone(), shutdown.clone()));
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(accept_loop(
        state.clone(),
        router.clone(),
        ipv4,
        acceptor.clone(),
        shutdown.clone(),
        true,
    ));
    if let Some(ipv6) = ipv6 {
        tasks.spawn(accept_loop(
            state,
            router,
            ipv6,
            acceptor,
            shutdown.clone(),
            false,
        ));
    }
    shutdown.cancelled().await;
    tasks.shutdown().await;
    watchdog.abort();
}

async fn accept_loop(
    state: Arc<Shared>,
    router: Router,
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
    shutdown: CancellationToken,
    required: bool,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, remote) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                // A single aborted connection must not take the listener down;
                // anything else is fatal and restarted by the application.
                if is_transient_accept_error(&error) {
                    tracing::debug!("transient accept error: {error}");
                    continue;
                }
                tracing::error!("listener failed: {error}");
                if required {
                    let _ = state.events.send(ServerEvent::ListenerFailed {
                        error: error.to_string(),
                    });
                }
                return;
            }
        };
        // Responses to discovery probes are tiny; Nagle would delay them.
        let _ = stream.set_nodelay(true);
        let router = router.clone();
        let acceptor = acceptor.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            serve_connection(router, stream, remote, acceptor, shutdown).await;
        });
    }
}

fn is_transient_accept_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::OutOfMemory
    )
}

async fn serve_connection(
    router: Router,
    stream: tokio::net::TcpStream,
    remote: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    shutdown: CancellationToken,
) {
    let ip = remote.ip();
    match acceptor {
        Some(acceptor) => {
            let tls = match acceptor.accept(stream).await {
                Ok(tls) => tls,
                Err(error) => {
                    tracing::debug!("TLS handshake with {remote} failed: {error}");
                    return;
                }
            };
            let cert_fingerprint = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .map(|cert| fingerprint_from_cert_der(cert.as_ref()));
            serve_http(
                TokioIo::new(tls),
                PeerInfo {
                    ip,
                    cert_fingerprint,
                },
                router,
                shutdown,
            )
            .await;
        }
        None => {
            serve_http(
                TokioIo::new(stream),
                PeerInfo {
                    ip,
                    cert_fingerprint: None,
                },
                router,
                shutdown,
            )
            .await;
        }
    }
}

async fn serve_http<I>(io: TokioIo<I>, peer: PeerInfo, router: Router, shutdown: CancellationToken)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = tower::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
        let router = router.clone();
        let peer = peer.clone();
        async move {
            // Handlers need the connection's identity; it is not part of the
            // request the peer sent.
            request.extensions_mut().insert(peer);
            Service::call(&mut router.clone(), request).await
        }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, hyper_util::service::TowerToHyperService::new(service));
    tokio::select! {
        result = connection => {
            if let Err(error) = result {
                tracing::debug!("connection closed with an error: {error}");
            }
        }
        _ = shutdown.cancelled() => {}
    }
}

/// Abandons sessions whose sender stopped sending.
async fn watch_sessions(state: Arc<Shared>, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        let mut slot = state.session.lock().await;
        let stale = matches!(
            slot.as_ref(),
            Some(Session::Active(session)) if session.last_activity.elapsed() > SESSION_IDLE_TIMEOUT
        );
        if stale {
            let session_id = slot
                .as_ref()
                .map(|session| session.id().to_string())
                .unwrap_or_default();
            *slot = None;
            drop(slot);
            tracing::info!("session {session_id} timed out");
            let _ = state.events.send(ServerEvent::SessionEnded {
                session_id,
                outcome: SessionOutcome::TimedOut,
            });
        }
    }
}
