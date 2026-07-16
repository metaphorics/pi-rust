use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
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

type PendingResult = Result<RpcResponseEnvelope, RpcProcessError>;

struct WriterMessage {
    line: String,
    request_id: Option<String>,
}

struct Inner {
    exited: AtomicBool,
    next_request_id: AtomicU64,
    next_subscriber_id: AtomicU64,
    stderr: Mutex<Vec<u8>>,
    pending: Mutex<HashMap<String, oneshot::Sender<PendingResult>>>,
    event_subscribers: Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>,
    exit_subscribers: Mutex<HashMap<u64, mpsc::UnboundedSender<RpcProcessError>>>,
    ui_handler: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    writer: mpsc::UnboundedSender<WriterMessage>,
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
            pending: Mutex::new(HashMap::new()),
            event_subscribers: Mutex::new(HashMap::new()),
            exit_subscribers: Mutex::new(HashMap::new()),
            ui_handler: Mutex::new(None),
            writer: writer_tx,
            terminate: terminate_tx,
            exit_notify: Notify::new(),
        });

        tokio::spawn(writer_loop(stdin, writer_rx, Arc::clone(&inner)));
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
            finish_child(status, &wait_inner).await;
        });

        Ok(Self { inner })
    }

    pub async fn send(
        &self,
        command: RpcCommandEnvelope,
    ) -> Result<RpcResponseEnvelope, RpcProcessError> {
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
        self.inner.pending.lock().await.insert(id.clone(), sender);

        if self.inner.exited.load(Ordering::Acquire) {
            if let Some(sender) = self.inner.pending.lock().await.remove(&id) {
                let error = self.not_running_error().await;
                let _ = sender.send(Err(error));
            }
        } else if self
            .inner
            .writer
            .send(WriterMessage {
                line,
                request_id: Some(id.clone()),
            })
            .is_err()
        {
            reject_pending(&self.inner, &id, RpcProcessError::new("broken pipe")).await;
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
        let _ = self.inner.writer.send(WriterMessage {
            line,
            request_id: None,
        });
        Ok(())
    }

    pub async fn set_ui_request_handler(&self, handler: Option<mpsc::UnboundedSender<Value>>) {
        *self.inner.ui_handler.lock().await = handler;
    }

    pub async fn subscribe_events(&self) -> mpsc::UnboundedReceiver<Value> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = self
            .inner
            .next_subscriber_id
            .fetch_add(1, Ordering::Relaxed);
        self.inner.event_subscribers.lock().await.insert(id, sender);
        receiver
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
        *self.inner.ui_handler.lock().await = None;
        reject_all_pending(&self.inner, RpcProcessError::new("RPC process disposed")).await;
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

async fn writer_loop(
    mut stdin: tokio::process::ChildStdin,
    mut receiver: mpsc::UnboundedReceiver<WriterMessage>,
    inner: Arc<Inner>,
) {
    while let Some(message) = receiver.recv().await {
        if let Err(error) = stdin.write_all(message.line.as_bytes()).await {
            if let Some(id) = message.request_id {
                reject_pending(&inner, &id, RpcProcessError::new(error.to_string())).await;
            }
            continue;
        }
        if let Err(error) = stdin.flush().await
            && let Some(id) = message.request_id
        {
            reject_pending(&inner, &id, RpcProcessError::new(error.to_string())).await;
        }
    }
}

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
                if let Some(pending) = inner.pending.lock().await.remove(&id) {
                    let _ = pending.send(Ok(response));
                }
            }
            ChildLine::UiRequest(request) => {
                let handler = inner.ui_handler.lock().await.clone();
                if let Some(handler) = handler {
                    let _ = handler.send(request);
                }
            }
            ChildLine::Event(event) => {
                inner
                    .event_subscribers
                    .lock()
                    .await
                    .retain(|_, subscriber| subscriber.send(event.clone()).is_ok());
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

async fn finish_child(status: std::io::Result<ExitStatus>, inner: &Arc<Inner>) {
    if inner.exited.swap(true, Ordering::AcqRel) {
        return;
    }
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
    reject_all_pending(inner, error.clone()).await;
    let mut subscribers = inner.exit_subscribers.lock().await;
    for subscriber in subscribers.values() {
        let _ = subscriber.send(error.clone());
    }
    subscribers.clear();
    inner.exit_notify.notify_waiters();
}

async fn reject_pending(inner: &Arc<Inner>, id: &str, error: RpcProcessError) {
    if let Some(pending) = inner.pending.lock().await.remove(id) {
        let _ = pending.send(Err(error));
    }
}

async fn reject_all_pending(inner: &Arc<Inner>, error: RpcProcessError) {
    let pending = std::mem::take(&mut *inner.pending.lock().await);
    for (_, sender) in pending {
        let _ = sender.send(Err(error.clone()));
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
