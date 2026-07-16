//! NDJSON RPC client over the sidecar's stdio (Phase 6 plan §2, §6).
//!
//! Strictness rules (plan §6 "Protocol integrity"):
//! - The first frame MUST be a valid `lifecycle/hello`; anything else
//!   (pollution, garbage, version mismatch, EOF) fails the handshake.
//! - After the handshake, a malformed line is logged and skipped — one bad
//!   line never kills the connection.
//! - Control frames (responses, pong, initialized) are routed inline by the
//!   reader and never queue behind application frames, so the control plane
//!   stays live even when the inbound queue is saturated.
//! - All queues are bounded. A sidecar flooding faster than the consumer
//!   drains blocks the reader, which backpressures the sidecar through its
//!   stdout pipe. A wedged consumer stops pong processing and trips the
//!   heartbeat, so nothing queues without bound.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pi_ext_protocol::{
    Envelope, HelloParams, InitializedParams, MAX_FRAME_BYTES, NegotiatedVersion, Notification,
    ProtocolError, Request, RequestId, ResponseResult, decode_frame, encode_frame,
};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::extensions::spawn::{SidecarProcess, kill_process_tree};
use crate::extensions::state::DeadReason;

/// Per-line byte cap for retained diagnostics (stderr is untrusted output).
const MAX_DIAGNOSTIC_LINE_BYTES: usize = 4096;

/// Transport tuning knobs. Defaults follow the Phase 6 plan (§6).
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Bound of the outbound writer queue.
    pub writer_queue: usize,
    /// Bound of the inbound application-frame queue.
    pub incoming_queue: usize,
    /// Heartbeat ping interval.
    pub heartbeat_interval: Duration,
    /// Consecutive unanswered pings before the sidecar is declared dead.
    pub heartbeat_misses: u32,
    /// Retained diagnostic lines (stderr + protocol warnings).
    pub max_diagnostics: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            writer_queue: 1024,
            incoming_queue: 1024,
            heartbeat_interval: Duration::from_secs(10),
            heartbeat_misses: 2,
            max_diagnostics: 256,
        }
    }
}

#[derive(Clone, Debug, Error)]
pub enum ClientError {
    #[error("sidecar connection closed: {0}")]
    Closed(String),
    #[error("sidecar request timed out after {0:?}")]
    Timeout(Duration),
    #[error("sidecar returned an error ({}): {}", .0.code, .0.message)]
    Remote(ProtocolError),
    #[error("failed to encode protocol frame: {0}")]
    Encode(String),
}

/// Application frames arriving from the sidecar, in wire order.
///
/// Control frames (responses, `lifecycle/pong`, `lifecycle/initialized`,
/// duplicate hellos) never appear here.
#[derive(Debug)]
pub enum Incoming {
    /// A sidecar-initiated request the host must answer via
    /// [`SidecarConnection::respond`].
    Request { id: RequestId, request: Request },
    /// A sidecar notification.
    Notification(Notification),
    /// The sidecar cancelled one of its own in-flight requests.
    Cancel { id: RequestId },
}

struct HeartbeatState {
    last_sent: u64,
    last_pong: u64,
}

struct PendingMap {
    closed: Option<DeadReason>,
    map: HashMap<u64, oneshot::Sender<Result<Value, ClientError>>>,
}

struct Shared {
    writer: mpsc::Sender<Vec<u8>>,
    pending: parking_lot::Mutex<PendingMap>,
    next_id: AtomicU64,
    close_tx: watch::Sender<Option<DeadReason>>,
    exit_tx: watch::Sender<Option<ExitStatus>>,
    heartbeat: parking_lot::Mutex<HeartbeatState>,
    diagnostics: parking_lot::Mutex<VecDeque<String>>,
    max_diagnostics: usize,
    initialized: parking_lot::Mutex<Option<InitializedParams>>,
    kill: Notify,
}

impl Shared {
    /// Diagnostics are untrusted output: cap each stored line's bytes.
    fn diag(&self, mut line: String) {
        if line.len() > MAX_DIAGNOSTIC_LINE_BYTES {
            let mut end = MAX_DIAGNOSTIC_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push('…');
        }
        let mut diagnostics = self.diagnostics.lock();
        if diagnostics.len() == self.max_diagnostics {
            diagnostics.pop_front();
        }
        diagnostics.push_back(line);
    }

    /// First close wins; rejects every pending request exactly once.
    fn close_once(&self, reason: DeadReason) {
        let drained = {
            let mut pending = self.pending.lock();
            if pending.closed.is_some() {
                return;
            }
            pending.closed = Some(reason.clone());
            std::mem::take(&mut pending.map)
        };
        let message = reason.to_string();
        for (_, tx) in drained {
            let _ = tx.send(Err(ClientError::Closed(message.clone())));
        }
        let _ = self.close_tx.send(Some(reason));
    }

    fn closed_reason(&self) -> Option<DeadReason> {
        self.pending.lock().closed.clone()
    }
}

/// One live sidecar process plus its RPC transport.
///
/// Dropping the last handle kills the process (kill-on-drop is the backstop;
/// the process group is killed explicitly).
pub struct SidecarConnection {
    shared: Arc<Shared>,
    hello: HelloParams,
    version: NegotiatedVersion,
}

impl std::fmt::Debug for SidecarConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarConnection")
            .field("hello", &self.hello)
            .field("closed", &self.shared.closed_reason())
            .finish_non_exhaustive()
    }
}

impl Drop for SidecarConnection {
    fn drop(&mut self) {
        self.shared.close_once(DeadReason::Killed);
        self.shared.kill.notify_one();
    }
}

impl SidecarConnection {
    /// The sidecar's `lifecycle/hello` payload.
    pub fn hello(&self) -> &HelloParams {
        &self.hello
    }

    pub fn negotiated_version(&self) -> NegotiatedVersion {
        self.version
    }

    /// `Some(reason)` once the connection is dead.
    pub fn closed(&self) -> Option<DeadReason> {
        self.shared.closed_reason()
    }

    /// Wait until the connection dies and return the reason.
    pub async fn wait_closed(&self) -> DeadReason {
        let mut rx = self.shared.close_tx.subscribe();
        loop {
            if let Some(reason) = rx.borrow_and_update().clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                // Sender lives in Shared, which we hold; unreachable in practice.
                return self
                    .shared
                    .closed_reason()
                    .unwrap_or(DeadReason::StdioClosed);
            }
        }
    }

    /// Exit status of the sidecar process, once reaped.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        *self.shared.exit_tx.borrow()
    }

    /// Wait until the monitor reaps the process and records its exit status.
    /// Returns `None` if the monitor is gone without ever recording one.
    pub async fn wait_exit(&self) -> Option<ExitStatus> {
        let mut rx = self.shared.exit_tx.subscribe();
        loop {
            if let Some(status) = *rx.borrow_and_update() {
                return Some(status);
            }
            if rx.changed().await.is_err() {
                return None;
            }
        }
    }

    /// The `lifecycle/initialized` payload, once received.
    pub fn initialized(&self) -> Option<InitializedParams> {
        self.shared.initialized.lock().clone()
    }

    /// Snapshot of retained diagnostics (stderr lines, skipped frames).
    pub fn diagnostics(&self) -> Vec<String> {
        self.shared.diagnostics.lock().iter().cloned().collect()
    }

    /// Ask the monitor to kill the process group.
    pub fn kill(&self) {
        self.shared.kill.notify_one();
    }

    /// Send a request and wait for its response with no deadline
    /// (tool/execute-class calls: cancellation via cancel frames only).
    pub async fn request(&self, request: Request) -> Result<Value, ClientError> {
        self.begin_request(request).await?.wait().await
    }

    /// Send a request with one deadline covering queue admission and the
    /// response. On timeout the pending slot is dropped and, if the frame was
    /// already sent, a best-effort cancel frame follows.
    pub async fn request_timeout(
        &self,
        request: Request,
        deadline: Duration,
    ) -> Result<Value, ClientError> {
        let sleep = tokio::time::sleep(deadline);
        tokio::pin!(sleep);
        // Admission can block on a saturated writer queue; the deadline must
        // cover it. Dropping the begin future mid-send is safe: PendingReply's
        // Drop retires the pending slot.
        let mut pending = tokio::select! {
            begun = self.begin_request(request) => begun?,
            _ = &mut sleep => return Err(ClientError::Timeout(deadline)),
        };
        tokio::select! {
            result = &mut pending => result,
            _ = &mut sleep => {
                pending.cancel().await;
                Err(ClientError::Timeout(deadline))
            }
        }
    }

    /// Start a request and return a handle that can await or cancel it.
    pub async fn begin_request(&self, request: Request) -> Result<PendingReply, ClientError> {
        let raw = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let id = NonZeroU64::new(raw).expect("request id counter starts at 1");
        let envelope = Envelope::Request { id, request };
        let bytes = encode_frame(&envelope).map_err(|e| ClientError::Encode(e.to_string()))?;
        let rx = {
            let mut pending = self.shared.pending.lock();
            if let Some(reason) = &pending.closed {
                return Err(ClientError::Closed(reason.to_string()));
            }
            let (tx, rx) = oneshot::channel();
            pending.map.insert(raw, tx);
            rx
        };
        let reply = PendingReply {
            id,
            rx,
            shared: Arc::clone(&self.shared),
        };
        // If this send (or this whole future) is abandoned, `reply`'s Drop
        // retires the pending slot.
        self.send_bytes(bytes).await?;
        Ok(reply)
    }

    /// Send a notification, awaiting writer-queue capacity (never dropped,
    /// never unbounded; callers must not sit on the render thread).
    pub async fn notify(&self, notification: Notification) -> Result<(), ClientError> {
        let envelope = Envelope::Event {
            event: notification,
        };
        let bytes = encode_frame(&envelope).map_err(|e| ClientError::Encode(e.to_string()))?;
        self.send_bytes(bytes).await
    }

    /// Answer a sidecar-initiated request ([`Incoming::Request`]).
    pub async fn respond(&self, id: RequestId, result: ResponseResult) -> Result<(), ClientError> {
        let envelope = Envelope::Response { id, result };
        let bytes = encode_frame(&envelope).map_err(|e| ClientError::Encode(e.to_string()))?;
        self.send_bytes(bytes).await
    }

    async fn send_bytes(&self, bytes: Vec<u8>) -> Result<(), ClientError> {
        self.shared.writer.send(bytes).await.map_err(|_| {
            let reason = self
                .shared
                .closed_reason()
                .unwrap_or(DeadReason::StdioClosed);
            ClientError::Closed(reason.to_string())
        })
    }
}

/// An in-flight request. Await it for the response, or [`cancel`](Self::cancel) it.
pub struct PendingReply {
    id: RequestId,
    rx: oneshot::Receiver<Result<Value, ClientError>>,
    shared: Arc<Shared>,
}

impl PendingReply {
    pub fn id(&self) -> RequestId {
        self.id
    }

    pub async fn wait(self) -> Result<Value, ClientError> {
        let mut this = self;
        (&mut this).await
    }

    /// Abandon the request: forget the pending slot and send a cancel frame.
    pub async fn cancel(self) {
        self.shared.pending.lock().map.remove(&self.id.get());
        if let Ok(bytes) = encode_frame(&Envelope::Cancel { id: self.id }) {
            let _ = self.shared.writer.send(bytes).await;
        }
    }
}

impl Drop for PendingReply {
    fn drop(&mut self) {
        // Ids are never reused; removing an already-resolved id is a no-op.
        self.shared.pending.lock().map.remove(&self.id.get());
    }
}

impl Future for PendingReply {
    type Output = Result<Value, ClientError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.rx)
            .poll(cx)
            .map(|received| match received {
                Ok(result) => result,
                Err(_) => {
                    let reason = self
                        .shared
                        .closed_reason()
                        .unwrap_or(DeadReason::StdioClosed)
                        .to_string();
                    Err(ClientError::Closed(reason))
                }
            })
    }
}

/// Errors establishing a connection (handshake phase).
#[derive(Debug, Error)]
#[error("{0}")]
pub struct HandshakeError(pub String);

impl SidecarConnection {
    /// Take a freshly spawned process through the `lifecycle/hello` handshake
    /// and start the transport tasks.
    ///
    /// `incoming` is the host-owned application-frame sink; it outlives this
    /// connection so a consumer can attach before the first spawn and survive
    /// respawns. On handshake failure the process is killed before returning.
    pub(crate) async fn establish(
        process: SidecarProcess,
        config: &ClientConfig,
        handshake_timeout: Duration,
        incoming: mpsc::Sender<Incoming>,
    ) -> Result<Arc<Self>, HandshakeError> {
        let SidecarProcess {
            child,
            pid,
            stdin,
            stdout,
            stderr,
        } = process;
        let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>(config.writer_queue);
        let (close_tx, _) = watch::channel(None);
        let shared = Arc::new(Shared {
            writer: writer_tx,
            pending: parking_lot::Mutex::new(PendingMap {
                closed: None,
                map: HashMap::new(),
            }),
            next_id: AtomicU64::new(1),
            close_tx,
            exit_tx: watch::channel(None).0,
            heartbeat: parking_lot::Mutex::new(HeartbeatState {
                last_sent: 0,
                last_pong: 0,
            }),
            diagnostics: parking_lot::Mutex::new(VecDeque::new()),
            max_diagnostics: config.max_diagnostics,
            initialized: parking_lot::Mutex::new(None),
            kill: Notify::new(),
        });

        tokio::spawn(monitor_task(child, pid, Arc::clone(&shared)));
        tokio::spawn(stderr_task(stderr, Arc::clone(&shared)));
        tokio::spawn(writer_task(writer_rx, stdin, Arc::clone(&shared)));

        let mut frames = FrameReader::new(BufReader::new(stdout), MAX_FRAME_BYTES);
        let handshake = handshake(&mut frames, handshake_timeout).await;
        let (hello, version) = match handshake {
            Ok(ok) => ok,
            Err(message) => {
                shared.close_once(DeadReason::HandshakeFailed(message.clone()));
                shared.kill.notify_one();
                return Err(HandshakeError(message));
            }
        };

        tokio::spawn(reader_task(frames, Arc::clone(&shared), incoming));
        tokio::spawn(heartbeat_task(
            Arc::clone(&shared),
            config.heartbeat_interval,
            config.heartbeat_misses,
        ));

        Ok(Arc::new(Self {
            shared,
            hello,
            version,
        }))
    }
}

async fn handshake(
    frames: &mut FrameReader<BufReader<tokio::process::ChildStdout>>,
    deadline: Duration,
) -> Result<(HelloParams, NegotiatedVersion), String> {
    let frame = tokio::time::timeout(deadline, frames.next())
        .await
        .map_err(|_| format!("no lifecycle/hello within {deadline:?}"))?
        .map_err(|error| format!("reading handshake frame: {error}"))?;
    let bytes = match frame {
        ReadFrame::Frame(bytes) => bytes,
        ReadFrame::Oversize(size) => return Err(format!("handshake frame of {size} bytes")),
        ReadFrame::Truncated(_) => {
            return Err("sidecar exited mid-frame before lifecycle/hello".to_string());
        }
        ReadFrame::Eof => return Err("sidecar exited before lifecycle/hello".to_string()),
    };
    let envelope = decode_frame(&bytes).map_err(|error| error.to_string())?;
    let Envelope::Event {
        event: Notification::LifecycleHello(hello),
    } = envelope
    else {
        return Err("first frame was not lifecycle/hello".to_string());
    };
    let version = hello.negotiate().map_err(|error| error.to_string())?;
    Ok((hello, version))
}

async fn reader_task(
    mut frames: FrameReader<BufReader<tokio::process::ChildStdout>>,
    shared: Arc<Shared>,
    incoming: mpsc::Sender<Incoming>,
) {
    let mut close_rx = shared.close_tx.subscribe();
    loop {
        let frame = tokio::select! {
            frame = frames.next() => frame,
            _ = close_rx.changed() => break,
        };
        match frame {
            Ok(ReadFrame::Frame(bytes)) => match decode_frame(&bytes) {
                Ok(envelope) => {
                    if handle_envelope(envelope, &shared, &incoming, &mut close_rx)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => shared.diag(format!("skipping malformed sidecar frame: {error}")),
            },
            Ok(ReadFrame::Oversize(size)) => {
                shared.diag(format!("skipping oversize sidecar frame ({size} bytes)"));
            }
            Ok(ReadFrame::Truncated(bytes)) => {
                shared.diag(format!(
                    "discarding truncated trailing sidecar frame ({} bytes)",
                    bytes.len()
                ));
            }
            Ok(ReadFrame::Eof) | Err(_) => {
                shared.close_once(DeadReason::StdioClosed);
                break;
            }
        }
    }
}

/// `Err(())` means the connection closed while delivering.
async fn handle_envelope(
    envelope: Envelope,
    shared: &Shared,
    incoming: &mpsc::Sender<Incoming>,
    close_rx: &mut watch::Receiver<Option<DeadReason>>,
) -> Result<(), ()> {
    let item = match envelope {
        Envelope::Response { id, result } => {
            let slot = shared.pending.lock().map.remove(&id.get());
            match slot {
                Some(tx) => {
                    let outcome = match result {
                        ResponseResult::Ok { ok } => Ok(ok),
                        ResponseResult::Err { err } => Err(ClientError::Remote(err)),
                    };
                    let _ = tx.send(outcome);
                }
                None => shared.diag(format!("response for unknown request id {id}")),
            }
            return Ok(());
        }
        Envelope::Event {
            event: Notification::LifecyclePong(pong),
        } => {
            // Only the currently outstanding nonce counts: a stale or
            // fabricated future nonce must never mark pings answered.
            let mut heartbeat = shared.heartbeat.lock();
            if pong.nonce == heartbeat.last_sent {
                heartbeat.last_pong = pong.nonce;
            } else {
                let (received, expected) = (pong.nonce, heartbeat.last_sent);
                drop(heartbeat);
                shared.diag(format!(
                    "ignoring pong with nonce {received} (outstanding ping is {expected})"
                ));
            }
            return Ok(());
        }
        Envelope::Event {
            event: Notification::LifecycleInitialized(params),
        } => {
            *shared.initialized.lock() = Some(params);
            return Ok(());
        }
        Envelope::Event {
            event: Notification::LifecycleHello(_),
        } => {
            shared.diag("ignoring duplicate lifecycle/hello".to_string());
            return Ok(());
        }
        Envelope::Event { event } => Incoming::Notification(event),
        Envelope::Request { id, request } => Incoming::Request { id, request },
        Envelope::Cancel { id } => Incoming::Cancel { id },
    };
    // Bounded delivery: block for capacity (backpressuring the sidecar through
    // its stdout pipe), but abort promptly if the connection dies meanwhile.
    tokio::select! {
        permit = incoming.reserve() => {
            match permit {
                Ok(permit) => permit.send(item),
                // Consumer gone: C6 dropped the receiver; drain to nowhere.
                Err(_) => shared.diag("dropping inbound frame: no incoming consumer".to_string()),
            }
            Ok(())
        }
        _ = close_rx.changed() => Err(()),
    }
}

async fn writer_task(
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut stdin: tokio::process::ChildStdin,
    shared: Arc<Shared>,
) {
    let mut close_rx = shared.close_tx.subscribe();
    loop {
        let bytes = tokio::select! {
            bytes = rx.recv() => match bytes {
                Some(bytes) => bytes,
                None => break,
            },
            _ = close_rx.changed() => break,
        };
        if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
            shared.close_once(DeadReason::StdioClosed);
            break;
        }
    }
}

async fn heartbeat_task(shared: Arc<Shared>, interval: Duration, max_misses: u32) {
    let mut close_rx = shared.close_tx.subscribe();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // the immediate first tick
    let mut misses = 0u32;
    let mut last_send_failed = false;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = close_rx.changed() => return,
        }
        let (sent, answered) = {
            let heartbeat = shared.heartbeat.lock();
            (
                heartbeat.last_sent,
                heartbeat.last_pong >= heartbeat.last_sent,
            )
        };
        if sent > 0 && !answered {
            misses += 1;
        } else if !last_send_failed {
            misses = 0;
        }
        if misses >= max_misses {
            shared.close_once(DeadReason::HeartbeatMissed);
            shared.kill.notify_one();
            return;
        }
        let nonce = sent + 1;
        let ping = Envelope::Event {
            event: Notification::LifecyclePing(pi_ext_protocol::HeartbeatParams { nonce }),
        };
        let Ok(bytes) = encode_frame(&ping) else {
            return;
        };
        // Never block the miss clock on a wedged writer: a full queue is
        // itself lack of progress and counts as a miss.
        match shared.writer.try_send(bytes) {
            Ok(()) => {
                last_send_failed = false;
                shared.heartbeat.lock().last_sent = nonce;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                last_send_failed = true;
                misses += 1;
                if misses >= max_misses {
                    shared.close_once(DeadReason::HeartbeatMissed);
                    shared.kill.notify_one();
                    return;
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

async fn monitor_task(mut child: tokio::process::Child, pid: Option<u32>, shared: Arc<Shared>) {
    tokio::select! {
        status = child.wait() => {
            if let Ok(status) = status {
                let _ = shared.exit_tx.send(Some(status));
                shared.close_once(DeadReason::Exited(status.code()));
            } else {
                shared.close_once(DeadReason::StdioClosed);
            }
        }
        _ = shared.kill.notified() => {
            kill_process_tree(&mut child, pid);
            if let Ok(status) = child.wait().await {
                let _ = shared.exit_tx.send(Some(status));
            }
            shared.close_once(DeadReason::Killed);
        }
    }
}

async fn stderr_task(stderr: tokio::process::ChildStderr, shared: Arc<Shared>) {
    let mut frames = FrameReader::new(BufReader::new(stderr), MAX_DIAGNOSTIC_LINE_BYTES);
    loop {
        match frames.next().await {
            Ok(ReadFrame::Frame(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                shared.diag(format!("[sidecar stderr] {}", text.trim_end()));
            }
            Ok(ReadFrame::Oversize(size)) => {
                shared.diag(format!("[sidecar stderr] <{size}-byte line truncated>"));
            }
            // stderr is diagnostics, not protocol: a missing final newline
            // still carries useful text.
            Ok(ReadFrame::Truncated(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                shared.diag(format!("[sidecar stderr] {}", text.trim_end()));
                break;
            }
            Ok(ReadFrame::Eof) | Err(_) => break,
        }
    }
}

/// One read outcome from [`FrameReader`].
pub(crate) enum ReadFrame {
    /// A complete newline-terminated line (terminator included).
    Frame(Vec<u8>),
    /// A line exceeding the reader's cap; its bytes were discarded.
    Oversize(usize),
    /// A non-empty tail at EOF with no terminator. Strict NDJSON: never a
    /// frame — the peer died mid-write.
    Truncated(Vec<u8>),
    Eof,
}

/// Newline-delimited reader with a hard per-line memory bound.
///
/// An overlong line is discarded incrementally (never buffered whole) and
/// reported as [`ReadFrame::Oversize`].
pub(crate) struct FrameReader<R> {
    inner: R,
    cap: usize,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    pub(crate) fn new(inner: R, cap: usize) -> Self {
        Self { inner, cap }
    }

    pub(crate) async fn next(&mut self) -> std::io::Result<ReadFrame> {
        let mut line: Vec<u8> = Vec::new();
        let mut skipped: usize = 0;
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return Ok(if skipped > 0 {
                    ReadFrame::Oversize(skipped)
                } else if line.is_empty() {
                    ReadFrame::Eof
                } else {
                    ReadFrame::Truncated(line)
                });
            }
            match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => {
                    if skipped > 0 {
                        skipped += index + 1;
                        self.inner.consume(index + 1);
                        return Ok(ReadFrame::Oversize(skipped));
                    }
                    if line.len() + index + 1 > self.cap {
                        let total = line.len() + index + 1;
                        self.inner.consume(index + 1);
                        return Ok(ReadFrame::Oversize(total));
                    }
                    line.extend_from_slice(&available[..=index]);
                    self.inner.consume(index + 1);
                    return Ok(ReadFrame::Frame(line));
                }
                None => {
                    let chunk = available.len();
                    if skipped > 0 {
                        skipped += chunk;
                    } else if line.len() + chunk > self.cap {
                        skipped = line.len() + chunk;
                        line = Vec::new();
                    } else {
                        line.extend_from_slice(available);
                    }
                    self.inner.consume(chunk);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_reader_bounds_memory_on_oversize_lines() {
        let mut giant = vec![b'x'; MAX_FRAME_BYTES + 10];
        giant.push(b'\n');
        giant.extend_from_slice(b"{\"ok\":1}\n");
        let mut reader = FrameReader::new(BufReader::new(giant.as_slice()), MAX_FRAME_BYTES);
        match reader.next().await.unwrap() {
            ReadFrame::Oversize(size) => assert!(size > MAX_FRAME_BYTES),
            _ => panic!("expected oversize"),
        }
        match reader.next().await.unwrap() {
            ReadFrame::Frame(bytes) => assert_eq!(bytes, b"{\"ok\":1}\n"),
            _ => panic!("expected frame"),
        }
        assert!(matches!(reader.next().await.unwrap(), ReadFrame::Eof));
    }

    #[tokio::test]
    async fn frame_reader_rejects_final_unterminated_tail() {
        let mut reader = FrameReader::new(BufReader::new(&b"{\"a\":1}"[..]), MAX_FRAME_BYTES);
        match reader.next().await.unwrap() {
            ReadFrame::Truncated(bytes) => assert_eq!(bytes, b"{\"a\":1}"),
            _ => panic!("expected truncated tail"),
        }
        assert!(matches!(reader.next().await.unwrap(), ReadFrame::Eof));
    }
}
