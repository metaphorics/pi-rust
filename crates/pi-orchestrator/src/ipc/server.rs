use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use super::protocol::{
    OrchestratorRequest, OrchestratorResponse, encode_message, parse_request_line,
};

pub type HandlerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait IpcRequestHandler: Send + Sync + 'static {
    fn handle(&self, request: OrchestratorRequest) -> HandlerFuture<'_, OrchestratorResponse>;

    fn open_rpc_stream<'a>(
        &'a self,
        instance_id: &'a str,
        events: RpcEventSink,
    ) -> HandlerFuture<'a, Option<Box<dyn RpcStreamHandler>>>;
}

pub trait RpcStreamHandler: Send {
    fn handle_request(&mut self, request: Value) -> HandlerFuture<'_, Result<(), String>>;
    fn close(&mut self);
}

#[derive(Clone)]
pub struct RpcEventSink {
    #[cfg(unix)]
    sender: tokio::sync::mpsc::UnboundedSender<String>,
}

impl RpcEventSink {
    #[cfg(unix)]
    fn new(sender: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self { sender }
    }

    pub fn send<T>(&self, message: &T) -> Result<(), EventSendError>
    where
        T: Serialize + ?Sized,
    {
        #[cfg(unix)]
        {
            let line = encode_message(message)?;
            self.sender.send(line).map_err(|_| EventSendError::Closed)
        }
        #[cfg(not(unix))]
        {
            let _ = message;
            Err(EventSendError::Unsupported)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventSendError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("IPC stream is closed")]
    Closed,
    #[error("Unix socket IPC is not supported on this platform")]
    Unsupported,
}

#[derive(Debug, thiserror::Error)]
pub enum IpcServerError {
    #[error("orchestrator is already running: {0}")]
    AlreadyRunning(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Unix socket IPC is not supported on this platform")]
    Unsupported,
}

pub struct IpcServer {
    socket_path: PathBuf,
    #[cfg(unix)]
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    #[cfg(unix)]
    task: Option<tokio::task::JoinHandle<()>>,
}

impl IpcServer {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn shutdown(mut self) -> Result<(), IpcServerError> {
        #[cfg(unix)]
        {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(task) = self.task.take() {
                let _ = task.await;
            }
            remove_socket_file(&self.socket_path)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(IpcServerError::Unsupported)
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

pub async fn start_ipc_server(
    handler: Arc<dyn IpcRequestHandler>,
) -> Result<IpcServer, IpcServerError> {
    start_ipc_server_at(crate::config::get_socket_path(), handler).await
}

#[cfg(unix)]
pub async fn start_ipc_server_at(
    socket_path: impl Into<PathBuf>,
    handler: Arc<dyn IpcRequestHandler>,
) -> Result<IpcServer, IpcServerError> {
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;

    let socket_path = socket_path.into();
    remove_stale_socket_if_needed(&socket_path).await?;
    let listener = UnixListener::bind(&socket_path)?;
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let handler = Arc::clone(&handler);
                        tokio::spawn(async move {
                            let _ = serve_connection(stream, handler).await;
                        });
                    }
                    Err(_) => break,
                },
            }
        }
    });

    Ok(IpcServer {
        socket_path,
        shutdown: Some(shutdown_tx),
        task: Some(task),
    })
}

#[cfg(not(unix))]
pub async fn start_ipc_server_at(
    socket_path: impl Into<PathBuf>,
    _handler: Arc<dyn IpcRequestHandler>,
) -> Result<IpcServer, IpcServerError> {
    let _ = socket_path.into();
    Err(IpcServerError::Unsupported)
}

#[cfg(unix)]
async fn serve_connection(
    stream: tokio::net::UnixStream,
    handler: Arc<dyn IpcRequestHandler>,
) -> Result<(), std::io::Error> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(line) = write_rx.recv().await {
            write_half.write_all(line.as_bytes()).await?;
        }
        write_half.shutdown().await
    });

    let mut line = String::new();
    let request = loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            drop(write_tx);
            let _ = writer.await;
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_request_line(trimmed) {
            Ok(request) => break request,
            Err(error) => {
                send_response(&write_tx, &OrchestratorResponse::error(error.to_string()));
                drop(write_tx);
                let _ = writer.await;
                return Ok(());
            }
        }
    };

    let Some(instance_id) = request.rpc_stream_instance_id().map(str::to_owned) else {
        let response = handler.handle(request).await;
        send_response(&write_tx, &response);
        drop(write_tx);
        let _ = writer.await;
        return Ok(());
    };

    let ready = handler.handle(request).await;
    if ready.rpc_ready_instance().is_none() {
        send_response(&write_tx, &ready);
        drop(write_tx);
        let _ = writer.await;
        return Ok(());
    }

    // Buffer events while the stream is opened. Forwarding starts only after
    // rpc_ready is queued, so rpc_ready is always the first stream message.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<String>();
    let Some(mut rpc_stream) = handler
        .open_rpc_stream(&instance_id, RpcEventSink::new(event_tx))
        .await
    else {
        send_response(
            &write_tx,
            &OrchestratorResponse::error(format!("Unknown instance: {instance_id}")),
        );
        drop(write_tx);
        let _ = writer.await;
        return Ok(());
    };

    send_response(&write_tx, &ready);
    let event_write_tx = write_tx.clone();
    let event_forwarder = tokio::spawn(async move {
        while let Some(line) = event_rx.recv().await {
            if event_write_tx.send(line).is_err() {
                break;
            }
        }
    });

    // Keep the same BufReader after the protocol upgrade. Any command lines
    // already buffered with rpc_stream are drained immediately by read_line.
    let read_result = loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break Ok(()),
            Ok(_) => {}
            Err(error) => break Err(error),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(request) => {
                if let Err(error) = rpc_stream.handle_request(request).await {
                    send_response(&write_tx, &OrchestratorResponse::error(error));
                }
            }
            Err(error) => {
                send_response(&write_tx, &OrchestratorResponse::error(error.to_string()));
            }
        }
    };

    rpc_stream.close();
    event_forwarder.abort();
    drop(write_tx);
    let _ = writer.await;
    read_result
}

#[cfg(unix)]
fn send_response(
    sender: &tokio::sync::mpsc::UnboundedSender<String>,
    response: &OrchestratorResponse,
) {
    if let Ok(line) = encode_message(response) {
        let _ = sender.send(line);
    }
}

#[cfg(unix)]
async fn remove_stale_socket_if_needed(socket_path: &Path) -> Result<(), IpcServerError> {
    if !socket_path.exists() {
        return Ok(());
    }

    match tokio::net::UnixStream::connect(socket_path).await {
        Ok(_) => Err(IpcServerError::AlreadyRunning(socket_path.to_path_buf())),
        Err(error) if is_stale_probe_error(error.kind()) => {
            remove_socket_file(socket_path)?;
            Ok(())
        }
        Err(error) => Err(IpcServerError::Io(error)),
    }
}

#[cfg(unix)]
fn is_stale_probe_error(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
    )
}

#[cfg(unix)]
fn remove_socket_file(socket_path: &Path) -> Result<(), std::io::Error> {
    match std::fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::is_stale_probe_error;
    use std::io::ErrorKind;

    #[test]
    fn stale_socket_probe_error_matrix_matches_node() {
        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::NotFound,
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
        ] {
            assert!(is_stale_probe_error(kind), "{kind:?}");
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidInput,
            ErrorKind::AddrInUse,
            ErrorKind::TimedOut,
        ] {
            assert!(!is_stale_probe_error(kind), "{kind:?}");
        }
    }
}
