use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use parking_lot::Mutex as SyncMutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::wire::{
    ChildLine, RpcCommandEnvelope, RpcResponseEnvelope, classify_child_line, encode_line,
};

#[derive(Clone, Debug)]
pub struct RpcProcessOptions {
    pub cwd: PathBuf,
    pub command_override: Option<(PathBuf, Vec<String>)>,
}

impl RpcProcessOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            command_override: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct RpcProcessError(String);

impl RpcProcessError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

type PendingResult = Result<(RpcResponseEnvelope, u64), RpcProcessError>;

struct WriterMessage {
    line: String,
    request_id: Option<String>,
}

/// A request awaiting its response line. `claimed_by` names the output lane
/// whose position fences the response: when the response line arrives, the
/// oneshot resolves with that lane's sequence number at that instant, so the
/// caller can wait until every earlier lane item has been delivered before
/// publishing the response.
struct PendingEntry {
    sender: oneshot::Sender<PendingResult>,
    claimed_by: Option<u64>,
}

/// One subscriber's ordered output lane. `seq` counts items pushed onto this
/// lane; only `stdout_loop` (the single classifier of child lines) advances
/// it, so lane order is exactly child stdout emission order.
struct LaneSender {
    sender: mpsc::UnboundedSender<(u64, Value)>,
    seq: u64,
}

struct Inner {
    exited: AtomicBool,
    next_request_id: AtomicU64,
    next_subscriber_id: AtomicU64,
    stderr: Mutex<Vec<u8>>,
    pending: SyncMutex<HashMap<String, PendingEntry>>,
    subscribers: SyncMutex<HashMap<u64, LaneSender>>,
    /// The subscriber currently receiving `extension_ui_request` lines
    /// (last claim wins; `None` drops them), mirroring the oracle's single
    /// `setUiRequestHandler` slot.
    ui_owner: SyncMutex<Option<u64>>,
    exit_subscribers: Mutex<HashMap<u64, mpsc::UnboundedSender<RpcProcessError>>>,
    writer: StdMutex<Option<mpsc::UnboundedSender<WriterMessage>>>,
    terminate: mpsc::UnboundedSender<()>,
    exit_notify: Notify,
}

#[derive(Clone)]
pub struct RpcProcessInstance {
    inner: Arc<Inner>,
}

impl RpcProcessInstance {
    pub fn spawn(options: RpcProcessOptions) -> Result<Self, RpcProcessError> {
        let (program, args) = match options.command_override {
            Some(command) => command,
            None => sibling_pi_command()?,
        };

        let mut child = Command::new(program)
            .args(args)
            .current_dir(options.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .map_err(|error| {
                RpcProcessError::new(format!("RPC process error: {error}. Stderr: "))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RpcProcessError::new("Failed to create RPC process stdio"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RpcProcessError::new("Failed to create RPC process stdio"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RpcProcessError::new("Failed to create RPC process stdio"))?;
        let pid = child.id();

        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let (terminate_tx, mut terminate_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            exited: AtomicBool::new(false),
            next_request_id: AtomicU64::new(0),
            next_subscriber_id: AtomicU64::new(0),
            stderr: Mutex::new(Vec::new()),
            pending: SyncMutex::new(HashMap::new()),
            subscribers: SyncMutex::new(HashMap::new()),
            ui_owner: SyncMutex::new(None),
            exit_subscribers: Mutex::new(HashMap::new()),
            writer: StdMutex::new(Some(writer_tx)),
            terminate: terminate_tx,
            exit_notify: Notify::new(),
        });

        let writer_task = tokio::spawn(writer_loop(stdin, writer_rx, Arc::clone(&inner)));
        tokio::spawn(stdout_loop(stdout, Arc::clone(&inner)));
        tokio::spawn(stderr_loop(stderr, Arc::clone(&inner)));

        let wait_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = terminate_rx.recv() => {
                    terminate_child(&mut child, pid);
                    child.wait().await
                }
            };
            finish_child(status, &wait_inner, writer_task).await;
        });

        Ok(Self { inner })
    }

    pub async fn send(
        &self,
        command: RpcCommandEnvelope,
    ) -> Result<RpcResponseEnvelope, RpcProcessError> {
        self.send_inner(command, None)
            .await
            .map(|(response, _)| response)
    }

    /// Send a command whose response is fenced against `subscriber`'s output
    /// lane. Returns the response plus the lane barrier: the lane sequence
    /// number of the last item the child emitted before this response. The
    /// caller must not publish the response until it has delivered every lane
    /// item up to that barrier, which preserves the child's total stdout
    /// order.
    pub async fn send_claimed(
        &self,
        command: RpcCommandEnvelope,
        subscriber: u64,
    ) -> Result<(RpcResponseEnvelope, u64), RpcProcessError> {
        self.send_inner(command, Some(subscriber)).await
    }

    async fn send_inner(
        &self,
        command: RpcCommandEnvelope,
        claimed_by: Option<u64>,
    ) -> Result<(RpcResponseEnvelope, u64), RpcProcessError> {
        if self.inner.exited.load(Ordering::Acquire) {
            return Err(self.not_running_error().await);
        }

        let (id, command) = command.into_value_with_generated_id(|| {
            let sequence = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
            format!("orchestrator_{sequence}_{}", Uuid::new_v4())
        });
        let line =
            encode_line(&command).map_err(|error| RpcProcessError::new(error.to_string()))?;
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .insert(id.clone(), PendingEntry { sender, claimed_by });

        if self.inner.exited.load(Ordering::Acquire) {
            let entry = self.inner.pending.lock().remove(&id);
            if let Some(entry) = entry {
                let error = self.not_running_error().await;
                let _ = entry.sender.send(Err(error));
            }
        } else {
            let sent = self
                .inner
                .writer
                .lock()
                .expect("writer lock poisoned")
                .as_ref()
                .is_some_and(|writer| {
                    writer
                        .send(WriterMessage {
                            line,
                            request_id: Some(id.clone()),
                        })
                        .is_ok()
                });
            if !sent {
                reject_pending(&self.inner, &id, RpcProcessError::new("broken pipe"));
            }
        }

        receiver
            .await
            .unwrap_or_else(|_| Err(RpcProcessError::new("RPC process disposed")))
    }

    pub fn handle_ui_response<T: serde::Serialize>(
        &self,
        response: &T,
    ) -> Result<(), RpcProcessError> {
        if self.inner.exited.load(Ordering::Acquire) {
            return Ok(());
        }
        let line =
            encode_line(response).map_err(|error| RpcProcessError::new(error.to_string()))?;
        if let Some(writer) = self
            .inner
            .writer
            .lock()
            .expect("writer lock poisoned")
            .as_ref()
        {
            let _ = writer.send(WriterMessage {
                line,
                request_id: None,
            });
        }
        Ok(())
    }

    pub fn subscribe_output(&self) -> (u64, mpsc::UnboundedReceiver<(u64, Value)>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = self
            .inner
            .next_subscriber_id
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .subscribers
            .lock()
            .insert(id, LaneSender { sender, seq: 0 });
        (id, receiver)
    }

    pub fn unsubscribe_output(&self, subscriber: u64) {
        self.inner.subscribers.lock().remove(&subscriber);
    }

    /// Route subsequent `extension_ui_request` lines to `subscriber`'s
    /// output lane (last claim wins).
    pub fn claim_ui(&self, subscriber: u64) {
        *self.inner.ui_owner.lock() = Some(subscriber);
    }

    /// Release the UI slot if `subscriber` still owns it.
    pub fn release_ui(&self, subscriber: u64) {
        let mut owner = self.inner.ui_owner.lock();
        if *owner == Some(subscriber) {
            *owner = None;
        }
    }

    pub async fn subscribe_exit(&self) -> mpsc::UnboundedReceiver<RpcProcessError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = self
            .inner
            .next_subscriber_id
            .fetch_add(1, Ordering::Relaxed);
        self.inner.exit_subscribers.lock().await.insert(id, sender);
        receiver
    }

    pub async fn stderr(&self) -> String {
        stderr_text(&self.inner).await
    }

    pub fn has_exited(&self) -> bool {
        self.inner.exited.load(Ordering::Acquire)
    }

    pub async fn dispose(&self) {
        *self.inner.ui_owner.lock() = None;
        reject_all_pending(&self.inner, RpcProcessError::new("RPC process disposed"));
        if self.inner.exited.load(Ordering::Acquire) {
            return;
        }

        let notified = self.inner.exit_notify.notified();
        if self.inner.terminate.send(()).is_err() {
            return;
        }
        if !self.inner.exited.load(Ordering::Acquire) {
            notified.await;
        }
    }

    async fn not_running_error(&self) -> RpcProcessError {
        RpcProcessError::new(format!(
            "RPC process is not running. Stderr: {}",
            stderr_text(&self.inner).await
        ))
    }
}

fn sibling_pi_command() -> Result<(PathBuf, Vec<String>), RpcProcessError> {
    let executable = std::env::current_exe()
        .map_err(|error| RpcProcessError::new(format!("RPC process error: {error}. Stderr: ")))?;
    let parent = executable.parent().ok_or_else(|| {
        RpcProcessError::new("RPC process error: current executable has no parent. Stderr: ")
    })?;
    let name = if cfg!(windows) { "pi.exe" } else { "pi" };
    Ok((parent.join(name), vec!["--mode".into(), "rpc".into()]))
}

async fn writer_loop<W>(
    mut stdin: W,
    mut receiver: mpsc::UnboundedReceiver<WriterMessage>,
    inner: Arc<Inner>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = receiver.recv().await {
        if let Err(error) = stdin.write_all(message.line.as_bytes()).await {
            finish_writer(error, message.request_id.as_deref(), &inner).await;
            return;
        }
        if let Err(error) = stdin.flush().await {
            finish_writer(error, message.request_id.as_deref(), &inner).await;
            return;
        }
    }
}

async fn finish_writer(error: std::io::Error, request_id: Option<&str>, inner: &Arc<Inner>) {
    if inner.exited.swap(true, Ordering::AcqRel) {
        return;
    }

    inner.writer.lock().expect("writer lock poisoned").take();
    let error = RpcProcessError::new(error.to_string());
    if let Some(id) = request_id {
        reject_pending(inner, id, error.clone());
    }
    reject_all_pending(inner, error.clone());

    let mut subscribers = inner.exit_subscribers.lock().await;
    for subscriber in subscribers.values() {
        let _ = subscriber.send(error.clone());
    }
    subscribers.clear();
    inner.exit_notify.notify_waiters();
    let _ = inner.terminate.send(());
}

/// The single classifier of child stdout lines. Dispatch order per line is
/// the ordering contract: events and UI requests are pushed onto subscriber
/// lanes (advancing each lane's sequence number) strictly in stdout order,
/// and a response resolves its pending oneshot carrying the claiming lane's
/// current sequence number as a barrier. No two of the `pending`,
/// `subscribers`, and `ui_owner` locks are ever held at once.
async fn stdout_loop(stdout: ChildStdout, inner: Arc<Inner>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = classify_child_line(line) else {
            continue;
        };
        match message {
            ChildLine::Response(response) => {
                let Some(id) = response.id.clone() else {
                    continue;
                };
                let entry = inner.pending.lock().remove(&id);
                if let Some(entry) = entry {
                    let barrier = entry
                        .claimed_by
                        .and_then(|subscriber| {
                            inner
                                .subscribers
                                .lock()
                                .get(&subscriber)
                                .map(|lane| lane.seq)
                        })
                        .unwrap_or(0);
                    let _ = entry.sender.send(Ok((response, barrier)));
                }
            }
            ChildLine::UiRequest(request) => {
                let owner = *inner.ui_owner.lock();
                if let Some(owner) = owner {
                    let mut subscribers = inner.subscribers.lock();
                    if let Some(lane) = subscribers.get_mut(&owner) {
                        lane.seq += 1;
                        if lane.sender.send((lane.seq, request)).is_err() {
                            subscribers.remove(&owner);
                        }
                    }
                }
            }
            ChildLine::Event(event) => {
                inner.subscribers.lock().retain(|_, lane| {
                    lane.seq += 1;
                    lane.sender.send((lane.seq, event.clone())).is_ok()
                });
            }
        }
    }
}

async fn stderr_loop(mut stderr: ChildStderr, inner: Arc<Inner>) {
    let mut chunk = [0_u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => inner.stderr.lock().await.extend_from_slice(&chunk[..read]),
        }
    }
}

async fn stderr_text(inner: &Inner) -> String {
    String::from_utf8_lossy(&inner.stderr.lock().await).into_owned()
}

async fn finish_child(
    status: std::io::Result<ExitStatus>,
    inner: &Arc<Inner>,
    writer_task: JoinHandle<()>,
) {
    if inner.exited.swap(true, Ordering::AcqRel) {
        return;
    }
    inner.writer.lock().expect("writer lock poisoned").take();
    let stderr = stderr_text(inner).await;
    let error = match status {
        Ok(status) => {
            let (code, signal) = exit_parts(status);
            RpcProcessError::new(format!(
                "RPC process exited (code={code} signal={signal}). Stderr: {stderr}"
            ))
        }
        Err(error) => RpcProcessError::new(format!("RPC process error: {error}. Stderr: {stderr}")),
    };
    reject_all_pending(inner, error.clone());
    let _ = writer_task.await;
    let mut subscribers = inner.exit_subscribers.lock().await;
    for subscriber in subscribers.values() {
        let _ = subscriber.send(error.clone());
    }
    subscribers.clear();
    inner.exit_notify.notify_waiters();
}

fn reject_pending(inner: &Arc<Inner>, id: &str, error: RpcProcessError) {
    let entry = inner.pending.lock().remove(id);
    if let Some(entry) = entry {
        let _ = entry.sender.send(Err(error));
    }
}

fn reject_all_pending(inner: &Arc<Inner>, error: RpcProcessError) {
    let pending = std::mem::take(&mut *inner.pending.lock());
    for (_, entry) in pending {
        let _ = entry.sender.send(Err(error.clone()));
    }
}

#[cfg(unix)]
fn terminate_child(_child: &mut Child, pid: Option<u32>) {
    use rustix::process::{Pid, Signal, kill_process};
    if let Some(pid) = pid.and_then(|pid| Pid::from_raw(pid as i32)) {
        let _ = kill_process(pid, Signal::TERM);
    }
}

#[cfg(windows)]
fn terminate_child(child: &mut Child, _pid: Option<u32>) {
    let _ = child.start_kill();
}

#[cfg(unix)]
fn exit_parts(status: ExitStatus) -> (String, String) {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(signal) => ("null".into(), signal_name(signal)),
        None => (
            status
                .code()
                .map_or_else(|| "null".into(), |code| code.to_string()),
            "null".into(),
        ),
    }
}

#[cfg(windows)]
fn exit_parts(status: ExitStatus) -> (String, String) {
    (
        status
            .code()
            .map_or_else(|| "null".into(), |code| code.to_string()),
        "null".into(),
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        16 => "SIGSTKFLT",
        17 => "SIGCHLD",
        18 => "SIGCONT",
        19 => "SIGSTOP",
        20 => "SIGTSTP",
        21 => "SIGTTIN",
        22 => "SIGTTOU",
        23 => "SIGURG",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        26 => "SIGVTALRM",
        27 => "SIGPROF",
        28 => "SIGWINCH",
        29 => "SIGIO",
        30 => "SIGPWR",
        31 => "SIGSYS",
        _ => return format!("SIG{signal}"),
    }
    .into()
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGEMT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGBUS",
        11 => "SIGSEGV",
        12 => "SIGSYS",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        16 => "SIGURG",
        17 => "SIGSTOP",
        18 => "SIGTSTP",
        19 => "SIGCONT",
        20 => "SIGCHLD",
        21 => "SIGTTIN",
        22 => "SIGTTOU",
        23 => "SIGIO",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        26 => "SIGVTALRM",
        27 => "SIGPROF",
        28 => "SIGWINCH",
        29 => "SIGINFO",
        30 => "SIGUSR1",
        31 => "SIGUSR2",
        _ => return format!("SIG{signal}"),
    }
    .into()
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn signal_name(signal: i32) -> String {
    format!("SIG{signal}")
}

#[cfg(all(test, unix))]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::*;

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum FailurePoint {
        Write,
        Flush,
    }

    struct FailingWriter(FailurePoint);

    fn broken_pipe() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "synthetic broken pipe")
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.0 == FailurePoint::Write {
                Poll::Ready(Err(broken_pipe()))
            } else {
                Poll::Ready(Ok(buffer.len()))
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.0 == FailurePoint::Flush {
                Poll::Ready(Err(broken_pipe()))
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn assert_writer_failure_teardown(failure_point: FailurePoint) {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let (terminate, _terminate_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            exited: AtomicBool::new(false),
            next_request_id: AtomicU64::new(0),
            next_subscriber_id: AtomicU64::new(0),
            stderr: Mutex::new(Vec::new()),
            pending: SyncMutex::new(HashMap::new()),
            subscribers: SyncMutex::new(HashMap::new()),
            ui_owner: SyncMutex::new(None),
            exit_subscribers: Mutex::new(HashMap::new()),
            writer: StdMutex::new(Some(writer_tx.clone())),
            terminate,
            exit_notify: Notify::new(),
        });
        let weak_inner = Arc::downgrade(&inner);
        let (result_tx, result_rx) = oneshot::channel();
        inner.pending.lock().insert(
            "request".into(),
            PendingEntry {
                sender: result_tx,
                claimed_by: None,
            },
        );

        let writer_task = tokio::spawn(writer_loop(
            FailingWriter(failure_point),
            writer_rx,
            Arc::clone(&inner),
        ));
        writer_tx
            .send(WriterMessage {
                line: "request\n".into(),
                request_id: Some("request".into()),
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), writer_task)
            .await
            .expect("writer task retained its receiver and Inner after I/O failure")
            .unwrap();
        assert!(
            inner.writer.lock().expect("writer lock poisoned").is_none(),
            "writer failure retained the stored sender"
        );
        assert_eq!(
            result_rx.await.unwrap().unwrap_err().to_string(),
            "synthetic broken pipe"
        );

        drop(inner);
        assert!(
            weak_inner.upgrade().is_none(),
            "completed writer task retained Inner"
        );
    }

    #[tokio::test]
    async fn write_failure_closes_sender_and_releases_writer_inner() {
        assert_writer_failure_teardown(FailurePoint::Write).await;
    }

    #[tokio::test]
    async fn flush_failure_closes_sender_and_releases_writer_inner() {
        assert_writer_failure_teardown(FailurePoint::Flush).await;
    }

    #[tokio::test]
    async fn dispose_releases_inner_after_writer_task_ends() {
        let process = RpcProcessInstance::spawn(RpcProcessOptions {
            cwd: std::env::temp_dir(),
            command_override: Some((
                PathBuf::from("python3"),
                vec![
                    "-u".into(),
                    "-c".into(),
                    "import time; time.sleep(60)".into(),
                ],
            )),
        })
        .unwrap();
        let inner = Arc::downgrade(&process.inner);

        tokio::time::timeout(std::time::Duration::from_secs(1), async move {
            process.dispose().await;
            assert!(process.has_exited(), "dispose returned before child exit");
            drop(process);

            while inner.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer task did not terminate and release Inner after child teardown");
    }
}
