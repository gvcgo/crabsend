//! End-to-end tests: the real sender-side client against the real receiver
//! over loopback.
//!
//! Every test binds port `0` and dials the port the server reports, talks only
//! to `127.0.0.1`, keeps downloads in a temporary directory and shuts the
//! server down at the end, so the suite is hermetic and order-independent.
//!
//! The receiver only decides through its event stream, so each test also plays
//! the application: it answers `ServerEvent::PrepareUpload` with an
//! [`UploadDecision`] and asserts on the rest of the events it observes.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crabsend_core::client::{ClientError, HttpClient, HttpTarget, PrepareOutcome};
use crabsend_core::crypto::{Identity, random_fingerprint, sha256_hex_bytes};
use crabsend_core::model::{DeviceType, FileDto, PrepareUploadRequest, ProtocolType, RegisterDto};
use crabsend_core::server::{Server, ServerConfig, ServerEvent, SessionOutcome, UploadDecision};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Time budget for one step; a step that needs longer than this is a bug.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Payload size of the transfer tests: large enough to span several writes and
/// progress events, small enough to keep the suite fast.
const PAYLOAD_SIZE: usize = 256 * 1024;

/// Alias the test servers announce.
const RECEIVER_ALIAS: &str = "receiver";

/// Alias the clients announce.
const SENDER_ALIAS: &str = "sender";

/// Port a client announces for its own (nonexistent) server.
const SENDER_PORT: u16 = 53317;

// ---------------------------------------------------------------------------
// Payload and model helpers
// ---------------------------------------------------------------------------

/// Deterministic pseudo-random bytes: the values do not matter, only that the
/// payload is not a repeat pattern.
fn payload(size: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0_u64;
    (0..size)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// Writes `bytes` to `dir/name` and returns the path.
fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("writing the source file");
    path
}

/// An identity for tests that do not care which one they use.
///
/// Generating an RSA key is the slowest part of these tests, so the common
/// identity is shared; tests that depend on *who* the peer is generate their
/// own.
static ANY_IDENTITY: LazyLock<Identity> =
    LazyLock::new(|| Identity::generate().expect("generating a test identity"));

fn any_identity() -> Identity {
    ANY_IDENTITY.clone()
}

/// The `info` object a sender announces itself with.
fn sender_info(fingerprint: String) -> RegisterDto {
    RegisterDto {
        alias: SENDER_ALIAS.to_string(),
        version: "2.2".to_string(),
        device_model: None,
        device_type: Some(DeviceType::Desktop),
        fingerprint,
        port: SENDER_PORT,
        protocol: ProtocolType::Https,
        download: false,
    }
}

/// One offered file; `sha256` is what the receiver is asked to verify.
fn file_dto(id: &str, name: &str, size: u64, sha256: Option<String>) -> FileDto {
    FileDto {
        id: id.to_string(),
        file_name: name.to_string(),
        size,
        file_type: "application/octet-stream".to_string(),
        sha256,
        preview: None,
        metadata: None,
    }
}

/// The body of a `prepare-upload` offering `files`.
fn upload_request(info: RegisterDto, files: Vec<FileDto>) -> PrepareUploadRequest {
    PrepareUploadRequest {
        info,
        files: files
            .into_iter()
            .map(|file| (file.id.clone(), file))
            .collect(),
    }
}

/// The upload token the receiver assigned to `file_id`.
fn token<'a>(files: &'a BTreeMap<String, String>, file_id: &str) -> &'a str {
    files
        .get(file_id)
        .unwrap_or_else(|| panic!("the receiver accepted no file {file_id}"))
        .as_str()
}

/// Unwraps an accepted `prepare-upload`.
fn accepted(outcome: PrepareOutcome) -> (String, BTreeMap<String, String>) {
    match outcome {
        PrepareOutcome::Accepted { session_id, files } => (session_id, files),
        other => panic!("expected the transfer to be accepted, got {other:?}"),
    }
}

/// Asserts a request failed with exactly `status`.
fn assert_status(error: ClientError, status: u16) {
    assert_eq!(
        error.status(),
        Some(status),
        "the peer should have answered {status}: {error}"
    );
}

/// Cumulative byte counts must never go backwards and must reach the end.
fn assert_progress(values: &[u64], expected: u64, what: &str) {
    assert!(!values.is_empty(), "{what} reported no progress at all");
    assert!(
        values.windows(2).all(|pair| pair[0] <= pair[1]),
        "{what} reported progress out of order: {values:?}"
    );
    assert_eq!(
        values[values.len() - 1],
        expected,
        "{what} never reported the whole file: {values:?}"
    );
}

/// The next event, waiting no longer than `deadline`.
async fn next_event_until(
    events: &mut mpsc::UnboundedReceiver<ServerEvent>,
    deadline: tokio::time::Instant,
    what: &str,
) -> ServerEvent {
    match tokio::time::timeout_at(deadline, events.recv()).await {
        Ok(Some(event)) => event,
        Ok(None) => panic!("the server closed its event channel while waiting for {what}"),
        Err(_) => panic!("the server sent no {what} within {STEP_TIMEOUT:?}"),
    }
}

fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + STEP_TIMEOUT
}

/// Uploads one file, returning the cumulative counts the sender reported.
async fn upload(
    client: &HttpClient,
    session_id: &str,
    file_id: &str,
    token: &str,
    source: &Path,
) -> Result<Vec<u64>, ClientError> {
    let reported = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&reported);
    let cancel = CancellationToken::new();
    client
        .upload(
            session_id,
            file_id,
            token,
            source,
            move |total| sink.lock().expect("progress lock").push(total),
            &cancel,
        )
        .await?;
    let values = reported.lock().expect("progress lock").clone();
    Ok(values)
}

// ---------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------

/// What the test's "application" answers an incoming request with.
enum Answer {
    /// Receive every offered file into this directory.
    Accept(PathBuf),
    /// Refuse the request.
    Decline,
}

/// A running server plus the test-side view of its event stream.
struct Harness {
    server: Server,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    /// Events already observed, in arrival order.
    seen: Vec<ServerEvent>,
    /// Prompts the test deliberately left pending, kept alive here.
    held: Vec<oneshot::Sender<UploadDecision>>,
    download: TempDir,
    /// Fingerprint announced over plain HTTP, where no certificate exists.
    http_fingerprint: String,
}

impl Harness {
    /// Starts a server on an ephemeral port; `Some(identity)` serves HTTPS.
    async fn start(identity: Option<Identity>, pin: Option<&str>, verify_checksums: bool) -> Self {
        let download = TempDir::new().expect("creating the download directory");
        let http_fingerprint = random_fingerprint();
        let (events, receiver) = mpsc::unbounded_channel();
        let server = Server::start(ServerConfig {
            port: 0,
            identity,
            http_fingerprint: http_fingerprint.clone(),
            alias: RECEIVER_ALIAS.to_string(),
            device_model: Some("test receiver".to_string()),
            device_type: Some(DeviceType::Desktop),
            pin: pin.map(str::to_string),
            verify_checksums,
            download_enabled: true,
            events,
        })
        .await
        .expect("starting the server");
        assert_ne!(server.port(), 0, "the server must report the port it bound");
        Self {
            server,
            events: receiver,
            seen: Vec::new(),
            held: Vec::new(),
            download,
            http_fingerprint,
        }
    }

    /// Stops the server.
    async fn shutdown(self) {
        self.server.shutdown().await;
    }

    /// Where a client reaches this server.
    fn target(&self) -> HttpTarget {
        HttpTarget::new(self.server.protocol(), "127.0.0.1", self.server.port())
    }

    fn download_path(&self) -> PathBuf {
        self.download.path().to_path_buf()
    }

    /// The next event, waiting no longer than [`STEP_TIMEOUT`].
    async fn next_event(&mut self) -> ServerEvent {
        next_event_until(&mut self.events, deadline(), "server event").await
    }

    /// Pulls everything that is already queued.
    fn poll(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.seen.push(event);
        }
    }

    /// Waits for the first event described by `what`.
    async fn wait_for(
        &mut self,
        what: &str,
        matches: impl Fn(&ServerEvent) -> bool,
    ) -> ServerEvent {
        let deadline = deadline();
        loop {
            let event = next_event_until(&mut self.events, deadline, what).await;
            if matches(&event) {
                return event;
            }
            self.seen.push(event);
        }
    }

    /// Runs `prepare_upload` while answering the receiver's decision prompt.
    async fn prepare(
        &mut self,
        client: &HttpClient,
        request: &PrepareUploadRequest,
        pin: Option<&str>,
        answer: Answer,
    ) -> Result<PrepareOutcome, ClientError> {
        let cancel = CancellationToken::new();
        let mut prepare = Box::pin(client.prepare_upload(request, pin, &cancel));
        let deadline = deadline();
        loop {
            let event = tokio::select! {
                result = &mut prepare => return result,
                event = next_event_until(&mut self.events, deadline, "transfer decision") => event,
            };
            self.answer(event, &answer);
        }
    }

    /// Sends a `prepare-upload` and accepts every offered file into the
    /// download directory.
    async fn prepare_accepting(
        &mut self,
        client: &HttpClient,
        request: &PrepareUploadRequest,
        pin: Option<&str>,
    ) -> Result<PrepareOutcome, ClientError> {
        let destination = self.download_path();
        self.prepare(client, request, pin, Answer::Accept(destination))
            .await
    }

    /// Performs the test-side decision for one prompt.
    fn answer(&mut self, event: ServerEvent, answer: &Answer) {
        match event {
            ServerEvent::PrepareUpload {
                files, decision, ..
            } => {
                let reply = match answer {
                    Answer::Accept(destination) => UploadDecision::Accept {
                        file_ids: files.iter().map(|file| file.id.clone()).collect(),
                        destination: destination.clone(),
                    },
                    Answer::Decline => UploadDecision::Decline,
                };
                // `UploadDecision` has no `Debug`, so `expect` is unavailable.
                if decision.send(reply).is_err() {
                    panic!("the sender stopped waiting for a decision");
                }
            }
            other => self.seen.push(other),
        }
    }

    /// Waits until a sender is waiting for a decision and leaves it waiting:
    /// the prompt is kept alive so the session stays pending.
    async fn hold_pending(&mut self) -> String {
        loop {
            match self.next_event().await {
                ServerEvent::PrepareUpload {
                    session_id,
                    decision,
                    ..
                } => {
                    self.held.push(decision);
                    return session_id;
                }
                other => self.seen.push(other),
            }
        }
    }

    /// Answers the prompt [`Harness::hold_pending`] left pending.
    fn release(&mut self, decision: UploadDecision) {
        let sender = self.held.pop().expect("no request is pending");
        if sender.send(decision).is_err() {
            panic!("the sender stopped waiting for a decision");
        }
    }

    /// Whether an `UploadStarted` was reported for one file.
    fn started(&self, session_id: &str, file_id: &str) -> bool {
        self.seen.iter().any(|event| {
            matches!(
                event,
                ServerEvent::UploadStarted { session_id: s, file_id: f }
                    if s.as_str() == session_id && f.as_str() == file_id
            )
        })
    }

    /// The progress the receiver reported for one file, in arrival order.
    fn progress(&self, session_id: &str, file_id: &str) -> Vec<u64> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                ServerEvent::UploadProgress {
                    session_id: s,
                    file_id: f,
                    transferred,
                } if s.as_str() == session_id && f.as_str() == file_id => Some(*transferred),
                _ => None,
            })
            .collect()
    }

    /// Where the receiver stored one file.
    fn stored_path(&self, session_id: &str, file_id: &str) -> Option<&Path> {
        self.seen.iter().find_map(|event| match event {
            ServerEvent::UploadFinished {
                session_id: s,
                file_id: f,
                path,
            } if s.as_str() == session_id && f.as_str() == file_id => Some(path.as_path()),
            _ => None,
        })
    }

    /// The error one file was recorded with.
    fn failure(&self, file_id: &str) -> Option<&str> {
        self.seen.iter().find_map(|event| match event {
            ServerEvent::UploadFailed {
                file_id: f, error, ..
            } if f.as_str() == file_id => Some(error.as_str()),
            _ => None,
        })
    }

    /// How a session ended, if it did.
    fn outcome(&self, session_id: &str) -> Option<SessionOutcome> {
        self.seen.iter().find_map(|event| match event {
            ServerEvent::SessionEnded {
                session_id: s,
                outcome,
            } if s.as_str() == session_id => Some(*outcome),
            _ => None,
        })
    }

    /// The peers the receiver announced as discovered.
    fn discovered(&self) -> Vec<(&RegisterDto, Option<&str>)> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                ServerEvent::Discovered { peer } => {
                    Some((&peer.info, peer.cert_fingerprint.as_deref()))
                }
                _ => None,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 1. Happy path over HTTPS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn https_transfers_a_file_and_reports_the_whole_session() {
    let receiver = Identity::generate().expect("generating the receiver identity");
    let sender = Identity::generate().expect("generating the sender identity");
    let mut harness = Harness::start(Some(receiver.clone()), None, true).await;
    assert_eq!(harness.server.protocol(), ProtocolType::Https);

    // The client pins the receiver's certificate fingerprint, so the handshake
    // itself proves which device is on the other end.
    let client = HttpClient::new(
        &sender,
        &harness.target(),
        Some(receiver.fingerprint.as_str()),
        Some(STEP_TIMEOUT),
    )
    .expect("building the client");

    let announced = sender_info(sender.fingerprint.clone());
    let peer = client
        .register(&announced)
        .await
        .expect("registering with the receiver");
    assert_eq!(peer.alias, RECEIVER_ALIAS);
    assert_eq!(peer.fingerprint, receiver.fingerprint);

    let discovered = harness
        .wait_for("Discovered", |event| {
            matches!(event, ServerEvent::Discovered { .. })
        })
        .await;
    match discovered {
        ServerEvent::Discovered { peer } => {
            assert_eq!(peer.info.fingerprint, sender.fingerprint);
            assert_eq!(
                peer.cert_fingerprint.as_deref(),
                Some(sender.fingerprint.as_str()),
                "the announced peer must be the certificate it presented"
            );
            assert_eq!(peer.address, IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        _ => unreachable!("waited for Discovered"),
    }

    let bytes = payload(PAYLOAD_SIZE);
    let source_dir = TempDir::new().expect("creating the source directory");
    let source = write_source(source_dir.path(), "payload.bin", &bytes);
    let request = upload_request(
        announced,
        vec![file_dto(
            "f1",
            "payload.bin",
            bytes.len() as u64,
            Some(sha256_hex_bytes(&bytes)),
        )],
    );

    let (session_id, files) = accepted(
        harness
            .prepare_accepting(&client, &request, None)
            .await
            .expect("the receiver must accept the transfer"),
    );
    let sender_progress = upload(&client, &session_id, "f1", token(&files, "f1"), &source)
        .await
        .expect("uploading the file");
    assert_progress(&sender_progress, bytes.len() as u64, "the sender");

    harness.poll();
    let stored = harness
        .stored_path(&session_id, "f1")
        .expect("UploadFinished must report where the file was stored")
        .to_path_buf();
    assert_eq!(
        stored.file_name().and_then(|name| name.to_str()),
        Some("payload.bin"),
        "the offered name must be the name on disk"
    );
    assert_eq!(stored.parent(), Some(harness.download_path().as_path()));
    assert_eq!(
        std::fs::read(&stored).expect("reading the received file"),
        bytes,
        "the received bytes must equal the source"
    );
    assert!(
        harness.started(&session_id, "f1"),
        "no UploadStarted arrived"
    );
    assert_progress(
        &harness.progress(&session_id, "f1"),
        bytes.len() as u64,
        "the receiver",
    );
    assert_eq!(
        harness.outcome(&session_id),
        Some(SessionOutcome::Completed),
        "the session must be reported as completed"
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. Checksums
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_wrong_checksum_is_rejected_and_leaves_no_file_behind() {
    let identity = any_identity();
    let mut harness = Harness::start(None, None, true).await;
    let client = HttpClient::new(&identity, &harness.target(), None, Some(STEP_TIMEOUT))
        .expect("building the client");

    let bytes = payload(PAYLOAD_SIZE);
    let source_dir = TempDir::new().expect("creating the source directory");
    let source = write_source(source_dir.path(), "payload.bin", &bytes);
    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto(
            "f1",
            "payload.bin",
            bytes.len() as u64,
            // The digest of something else entirely.
            Some(sha256_hex_bytes(b"not the payload")),
        )],
    );

    let (session_id, files) = accepted(
        harness
            .prepare_accepting(&client, &request, None)
            .await
            .expect("the checksum is only checked when the bytes arrive"),
    );
    let error = upload(&client, &session_id, "f1", token(&files, "f1"), &source)
        .await
        .expect_err("a mismatching checksum must fail the upload");
    assert_status(error, 422);

    harness.poll();
    let failure = harness
        .failure("f1")
        .expect("UploadFailed must be reported");
    assert!(
        failure.starts_with("checksum mismatch"),
        "unexpected failure: {failure}"
    );
    let left_behind: Vec<_> = std::fs::read_dir(harness.download_path())
        .expect("listing the download directory")
        .map(|entry| entry.expect("reading a directory entry").file_name())
        .collect();
    assert!(
        left_behind.is_empty(),
        "a rejected file must not be left behind: {left_behind:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn a_correct_checksum_is_accepted() {
    let identity = any_identity();
    let mut harness = Harness::start(None, None, true).await;
    let client = HttpClient::new(&identity, &harness.target(), None, Some(STEP_TIMEOUT))
        .expect("building the client");

    let bytes = payload(PAYLOAD_SIZE);
    let source_dir = TempDir::new().expect("creating the source directory");
    let source = write_source(source_dir.path(), "payload.bin", &bytes);
    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto(
            "f1",
            "payload.bin",
            bytes.len() as u64,
            Some(sha256_hex_bytes(&bytes)),
        )],
    );

    let (session_id, files) = accepted(
        harness
            .prepare_accepting(&client, &request, None)
            .await
            .expect("the receiver must accept the transfer"),
    );
    upload(&client, &session_id, "f1", token(&files, "f1"), &source)
        .await
        .expect("a matching checksum must be accepted");

    harness.poll();
    assert_eq!(harness.failure("f1"), None, "the file must not have failed");
    assert_eq!(
        std::fs::read(harness.download_path().join("payload.bin")).expect("reading the file"),
        bytes
    );
    assert_eq!(
        harness.outcome(&session_id),
        Some(SessionOutcome::Completed)
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. Decline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declined_transfer_is_forbidden_and_releases_the_slot() {
    let identity = any_identity();
    let mut harness = Harness::start(None, None, false).await;
    let client = HttpClient::new(&identity, &harness.target(), None, Some(STEP_TIMEOUT))
        .expect("building the client");

    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto("f1", "note.txt", 5, None)],
    );
    let error = harness
        .prepare(&client, &request, None, Answer::Decline)
        .await
        .expect_err("a declined request must fail");
    assert_status(error, 403);
    assert!(
        std::fs::read_dir(harness.download_path())
            .expect("listing the download directory")
            .next()
            .is_none(),
        "a declined transfer must not create any file"
    );

    // The refusal must not leak the single session slot.
    let (_, files) = accepted(
        harness
            .prepare_accepting(&client, &request, None)
            .await
            .expect("the slot must be free again"),
    );
    assert_eq!(files.len(), 1);

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. PIN
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_or_wrong_pin_is_rejected_and_attempts_are_throttled() {
    let identity = any_identity();
    let mut harness = Harness::start(None, Some("1234"), false).await;
    let client = HttpClient::new(&identity, &harness.target(), None, Some(STEP_TIMEOUT))
        .expect("building the client");

    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto("f1", "note.txt", 5, None)],
    );

    // No PIN at all: the peer never even learns whether the transfer would be
    // wanted, so the application is not asked.
    let missing = harness
        .prepare(&client, &request, None, Answer::Decline)
        .await
        .expect_err("a PIN is required");
    assert_status(missing, 401);

    // Three wrong attempts in a row are each answered, and each one is one
    // failure closer to the throttle.
    for _ in 1..=3 {
        let wrong = harness
            .prepare(&client, &request, Some("0000"), Answer::Decline)
            .await
            .expect_err("a wrong PIN must be rejected");
        assert_status(wrong, 401);
    }

    // The third failure exhausts the budget for this address: from now on even
    // the right PIN is refused without being looked at.
    let throttled = harness
        .prepare(&client, &request, Some("1234"), Answer::Decline)
        .await
        .expect_err("a throttled request must fail");
    assert_status(throttled, 429);

    harness.poll();
    assert!(
        harness.held.is_empty(),
        "the receiver must not ask the application about a request that failed the PIN check"
    );
    assert!(
        harness
            .seen
            .iter()
            .all(|event| !matches!(event, ServerEvent::PrepareUpload { .. })),
        "no prompt may reach the application while the PIN is wrong"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn the_configured_pin_lets_a_transfer_through() {
    let identity = any_identity();
    // A fresh server: the attempt counter of the throttling test must not leak
    // into this one.
    let mut harness = Harness::start(None, Some("1234"), false).await;
    let client = HttpClient::new(&identity, &harness.target(), None, Some(STEP_TIMEOUT))
        .expect("building the client");

    let bytes = b"hello pin".to_vec();
    let source_dir = TempDir::new().expect("creating the source directory");
    let source = write_source(source_dir.path(), "note.txt", &bytes);
    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto("f1", "note.txt", bytes.len() as u64, None)],
    );

    let (session_id, files) = accepted(
        harness
            .prepare_accepting(&client, &request, Some("1234"))
            .await
            .expect("the configured PIN must be accepted"),
    );
    upload(&client, &session_id, "f1", token(&files, "f1"), &source)
        .await
        .expect("the transfer must go through");

    harness.poll();
    assert_eq!(
        std::fs::read(harness.download_path().join("note.txt")).expect("reading the file"),
        bytes
    );
    assert_eq!(
        harness.outcome(&session_id),
        Some(SessionOutcome::Completed)
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5. Busy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_transfer_while_one_is_pending_is_a_conflict() {
    let identity = any_identity();
    let mut harness = Harness::start(None, None, false).await;
    let target = harness.target();
    let first = Arc::new(
        HttpClient::new(&identity, &target, None, Some(STEP_TIMEOUT)).expect("building a client"),
    );
    let second = HttpClient::new(&identity, &target, None, Some(STEP_TIMEOUT))
        .expect("building a second client");

    let bytes = b"first".to_vec();
    let source_dir = TempDir::new().expect("creating the source directory");
    let source = write_source(source_dir.path(), "first.txt", &bytes);
    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto("f1", "first.txt", bytes.len() as u64, None)],
    );

    let cancel = CancellationToken::new();
    let in_flight = tokio::spawn({
        let first = Arc::clone(&first);
        let request = request.clone();
        async move { first.prepare_upload(&request, None, &cancel).await }
    });
    let pending = harness.hold_pending().await;
    assert!(!pending.is_empty(), "the receiver must name the session");

    let busy = tokio::time::timeout(
        STEP_TIMEOUT,
        second.prepare_upload(&request, None, &CancellationToken::new()),
    )
    .await
    .expect("the conflict answer must not hang")
    .expect_err("a second concurrent transfer must be refused");
    assert_status(busy, 409);

    // Answering the first request frees the slot for the sender that was
    // turned away.
    harness.release(UploadDecision::Decline);
    let declined = in_flight
        .await
        .expect("the in-flight request must not panic")
        .expect_err("a declined request must fail");
    assert_status(declined, 403);

    let (session_id, files) = accepted(
        harness
            .prepare_accepting(&second, &request, None)
            .await
            .expect("the released slot must accept the transfer"),
    );
    upload(&second, &session_id, "f1", token(&files, "f1"), &source)
        .await
        .expect("uploading the file");
    harness.poll();
    assert_eq!(
        std::fs::read(harness.download_path().join("first.txt")).expect("reading the file"),
        bytes
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 6. File names
// ---------------------------------------------------------------------------

#[tokio::test]
async fn offered_names_cannot_escape_the_download_directory() {
    let identity = any_identity();
    let mut harness = Harness::start(None, None, false).await;
    let client = HttpClient::new(&identity, &harness.target(), None, Some(STEP_TIMEOUT))
        .expect("building the client");

    let sandbox = TempDir::new().expect("creating the sandbox");
    let destination = sandbox.path().join("downloads");
    let escaped = b"escaped".to_vec();
    let nested = b"nested".to_vec();
    let source_dir = TempDir::new().expect("creating the source directory");
    let escaped_source = write_source(source_dir.path(), "escape.txt", &escaped);
    let nested_source = write_source(source_dir.path(), "note.txt", &nested);

    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![
            file_dto("escape", "../../escape.txt", escaped.len() as u64, None),
            file_dto("nested", "folder/note.txt", nested.len() as u64, None),
        ],
    );
    let (session_id, files) = accepted(
        harness
            .prepare(&client, &request, None, Answer::Accept(destination.clone()))
            .await
            .expect("the receiver must accept the transfer"),
    );
    upload(
        &client,
        &session_id,
        "escape",
        token(&files, "escape"),
        &escaped_source,
    )
    .await
    .expect("uploading the flattened file");
    upload(
        &client,
        &session_id,
        "nested",
        token(&files, "nested"),
        &nested_source,
    )
    .await
    .expect("uploading the nested file");

    harness.poll();
    assert_eq!(
        std::fs::read(destination.join("escape.txt")).expect("the flattened file must be inside"),
        escaped
    );
    assert_eq!(
        std::fs::read(destination.join("folder").join("note.txt"))
            .expect("the nested file must be inside"),
        nested
    );
    assert!(
        !sandbox.path().join("escape.txt").exists(),
        "a parent-directory name must not be followed out of the download directory"
    );
    let sandbox_entries: Vec<_> = std::fs::read_dir(sandbox.path())
        .expect("listing the sandbox")
        .map(|entry| entry.expect("reading a directory entry").file_name())
        .collect();
    assert_eq!(sandbox_entries, vec![std::ffi::OsString::from("downloads")]);

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 7. Plain HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_http_serves_a_transfer_without_tls() {
    let identity = any_identity();
    let mut harness = Harness::start(None, None, false).await;
    assert_eq!(harness.server.protocol(), ProtocolType::Http);
    let announced_fingerprint = harness.http_fingerprint.clone();

    let target = harness.target();
    assert!(
        target.base_url().starts_with("http://"),
        "the plain server must be dialed without TLS: {}",
        target.base_url()
    );
    let client =
        HttpClient::new(&identity, &target, None, Some(STEP_TIMEOUT)).expect("building the client");
    assert_eq!(client.protocol(), ProtocolType::Http);
    assert_eq!(
        client.info().await.expect("probing the server").fingerprint,
        announced_fingerprint,
        "an HTTP server announces the fingerprint it was configured with"
    );

    let bytes = payload(PAYLOAD_SIZE);
    let source_dir = TempDir::new().expect("creating the source directory");
    let source = write_source(source_dir.path(), "payload.bin", &bytes);
    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto("f1", "payload.bin", bytes.len() as u64, None)],
    );

    let (session_id, files) = accepted(
        harness
            .prepare_accepting(&client, &request, None)
            .await
            .expect("the receiver must accept the transfer"),
    );
    upload(&client, &session_id, "f1", token(&files, "f1"), &source)
        .await
        .expect("uploading the file");

    harness.poll();
    assert_eq!(
        std::fs::read(harness.download_path().join("payload.bin")).expect("reading the file"),
        bytes
    );
    assert_eq!(
        harness.outcome(&session_id),
        Some(SessionOutcome::Completed)
    );

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 8. Cancel while pending
// ---------------------------------------------------------------------------

/// Drives the cancel endpoint the way a peer that has not seen a session id
/// yet does: with no query parameters at all.
///
/// `HttpClient::cancel("")` cannot express this, because it always writes
/// `?sessionId=`, and the receiver then holds an empty id rather than no id —
/// which [`server::cancel`] must not attribute to the pending session. The
/// request is therefore written onto the wire by hand, against a plain-HTTP
/// receiver (see the test's assertions for what each form does).
async fn cancel_without_a_session_id(target: &HttpTarget) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect((target.host.as_str(), target.port))
        .await
        .expect("connecting to the receiver");
    // POST /api/localsend/v2/cancel with no body and no query string.
    let request = format!(
        "POST /api/localsend/v2/cancel HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\n\r\n",
        target.host
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("sending the cancel");
    let mut response = [0_u8; 256];
    let read = tokio::time::timeout(STEP_TIMEOUT, stream.read(&mut response))
        .await
        .expect("the receiver must answer the cancel")
        .expect("reading the cancel response");
    let response = String::from_utf8_lossy(&response[..read]).to_string();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the receiver must accept the cancel: {response}"
    );
}

/// A sender that gives up before it ever sees a session id can still free the
/// receiver's single session slot: the receiver attributes the cancel to the
/// peer address, because that is the only thing the sender can name.
#[tokio::test]
async fn a_cancel_from_the_waiting_peer_frees_the_pending_session() {
    let identity = any_identity();
    let mut harness = Harness::start(None, None, false).await;
    let client = HttpClient::new(&identity, &harness.target(), None, Some(STEP_TIMEOUT))
        .expect("building the client");

    let request = upload_request(
        sender_info(identity.fingerprint.clone()),
        vec![file_dto("f1", "note.txt", 5, None)],
    );

    let cancel = CancellationToken::new();
    let mut prepare = Box::pin(client.prepare_upload(&request, None, &cancel));
    // Keep polling the request until the receiver asks for a decision; the
    // prompt is held so the session stays pending.
    let session_id = loop {
        let event = tokio::select! {
            result = &mut prepare => {
                panic!("prepare-upload returned before the receiver asked for a decision: {result:?}")
            }
            event = harness.next_event() => event,
        };
        match event {
            ServerEvent::PrepareUpload {
                session_id,
                decision,
                ..
            } => {
                harness.held.push(decision);
                break session_id;
            }
            other => harness.seen.push(other),
        }
    };

    // The sender walks away without knowing the session id. An *empty* id is
    // not the same as no id: the receiver holds an id that matches nothing, so
    // the request stays pending.
    client.cancel("").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut prepare)
            .await
            .is_err(),
        "an empty session id must not end a session it cannot name"
    );

    // A cancel with no session parameter at all is what the receiver reads as
    // "the peer at this address gave up": the request comes back refused and
    // the session ends.
    cancel_without_a_session_id(&harness.target()).await;
    let refused = tokio::time::timeout(STEP_TIMEOUT, &mut prepare)
        .await
        .expect("a cancelled request must not be left hanging")
        .expect_err("a cancelled request must fail");
    assert_status(refused, 403);

    harness.poll();
    assert_eq!(
        harness.outcome(&session_id),
        Some(SessionOutcome::Cancelled),
        "the cancel must end the pending session"
    );

    // The slot is free again for the next sender.
    let (_, files) = accepted(
        harness
            .prepare_accepting(&client, &request, None)
            .await
            .expect("the slot must be free after the cancel"),
    );
    assert_eq!(files.len(), 1);

    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 9. Unproven identity claims
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fingerprint_its_certificate_does_not_prove_is_not_discovered() {
    let receiver = Identity::generate().expect("generating the receiver identity");
    let sender = Identity::generate().expect("generating the sender identity");
    let mut harness = Harness::start(Some(receiver.clone()), None, false).await;
    let client = HttpClient::new(
        &sender,
        &harness.target(),
        Some(receiver.fingerprint.as_str()),
        Some(STEP_TIMEOUT),
    )
    .expect("building the client");

    // The certificate this client presents is `sender`, but it claims to be
    // somebody else.
    let mut claim = sender_info(random_fingerprint());
    assert_ne!(claim.fingerprint, sender.fingerprint);
    client
        .register(&claim)
        .await
        .expect("the receiver still answers an unproven registration");
    harness.poll();
    assert!(
        harness.discovered().is_empty(),
        "an unproven fingerprint claim must not be announced as a peer"
    );

    // Claiming what the certificate proves is announced, with the proven
    // fingerprint rather than the claimed one.
    claim.fingerprint = sender.fingerprint.clone();
    client
        .register(&claim)
        .await
        .expect("registering with a proven fingerprint");
    harness.poll();
    let peers = harness.discovered();
    assert_eq!(peers.len(), 1, "the proven registration must be announced");
    let (info, cert_fingerprint) = peers[0];
    assert_eq!(info.fingerprint, sender.fingerprint);
    assert_eq!(
        cert_fingerprint,
        Some(sender.fingerprint.as_str()),
        "the announced identity must be the certificate, not the claim"
    );

    harness.shutdown().await;
}
