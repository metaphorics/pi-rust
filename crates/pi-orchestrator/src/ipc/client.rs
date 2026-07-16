use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::protocol::{
    InstanceSummary, OrchestratorRequest, OrchestratorResponse, encode_message, parse_response_line,
};

#[derive(Debug, thiserror::Error)]
pub enum IpcClientError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("Orchestrator socket closed before a response was received: {0}")]
    ClosedBeforeResponse(PathBuf),
    #[error("expected rpc_ready as the first stream message")]
    ExpectedRpcReady,
    #[error("Unix socket IPC is not supported on this platform")]
    Unsupported,
}

pub async fn send_ipc_request(
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, IpcClientError> {
    send_ipc_request_to(crate::config::get_socket_path(), request).await
}

#[cfg(unix)]
pub async fn send_ipc_request_to(
    socket_path: impl AsRef<Path>,
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, IpcClientError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let socket_path = socket_path.as_ref();
    let stream = UnixStream::connect(socket_path).await?;
    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(encode_message(request)?.as_bytes())
        .await?;
    write_half.shutdown().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Err(IpcClientError::ClosedBeforeResponse(
                socket_path.to_path_buf(),
            ));
        }
        if !line.trim().is_empty() {
            return Ok(parse_response_line(line.trim())?);
        }
    }
}

#[cfg(not(unix))]
pub async fn send_ipc_request_to(
    _socket_path: impl AsRef<Path>,
    _request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, IpcClientError> {
    Err(IpcClientError::Unsupported)
}

#[cfg(unix)]
pub struct RpcStreamClient {
    reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

#[cfg(not(unix))]
pub struct RpcStreamClient;

impl RpcStreamClient {
    #[cfg(unix)]
    pub async fn send<T>(&mut self, message: &T) -> Result<(), IpcClientError>
    where
        T: Serialize + ?Sized,
    {
        use tokio::io::AsyncWriteExt;

        self.writer
            .write_all(encode_message(message)?.as_bytes())
            .await?;
        Ok(())
    }

    #[cfg(not(unix))]
    pub async fn send<T>(&mut self, _message: &T) -> Result<(), IpcClientError>
    where
        T: Serialize + ?Sized,
    {
        Err(IpcClientError::Unsupported)
    }

    #[cfg(unix)]
    pub async fn next_message(&mut self) -> Result<Option<Value>, IpcClientError> {
        use tokio::io::AsyncBufReadExt;

        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line).await? == 0 {
                return Ok(None);
            }
            if !line.trim().is_empty() {
                return Ok(Some(serde_json::from_str(line.trim())?));
            }
        }
    }

    #[cfg(not(unix))]
    pub async fn next_message(&mut self) -> Result<Option<Value>, IpcClientError> {
        Err(IpcClientError::Unsupported)
    }
}

pub async fn connect_rpc_stream(
    instance_id: impl Into<String>,
) -> Result<(InstanceSummary, RpcStreamClient), IpcClientError> {
    connect_rpc_stream_to(crate::config::get_socket_path(), instance_id).await
}

#[cfg(unix)]
pub async fn connect_rpc_stream_to(
    socket_path: impl AsRef<Path>,
    instance_id: impl Into<String>,
) -> Result<(InstanceSummary, RpcStreamClient), IpcClientError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let socket_path = socket_path.as_ref().to_path_buf();
    let stream = UnixStream::connect(&socket_path).await?;
    let (read_half, mut writer) = stream.into_split();
    let request = OrchestratorRequest::RpcStream {
        instance_id: instance_id.into(),
    };
    writer
        .write_all(encode_message(&request)?.as_bytes())
        .await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let response = loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Err(IpcClientError::ClosedBeforeResponse(socket_path.clone()));
        }
        if !line.trim().is_empty() {
            break parse_response_line(line.trim())?;
        }
    };
    let instance = response
        .rpc_ready_instance()
        .cloned()
        .ok_or(IpcClientError::ExpectedRpcReady)?;
    Ok((instance, RpcStreamClient { reader, writer }))
}

#[cfg(not(unix))]
pub async fn connect_rpc_stream_to(
    _socket_path: impl AsRef<Path>,
    _instance_id: impl Into<String>,
) -> Result<(InstanceSummary, RpcStreamClient), IpcClientError> {
    Err(IpcClientError::Unsupported)
}
