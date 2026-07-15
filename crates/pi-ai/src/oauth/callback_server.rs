//! Minimal local HTTP callback server for browser OAuth PKCE flows.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

use super::oauth_page::{oauth_error_html, oauth_success_html};
use crate::auth::OAuthError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub state: Option<String>,
}

pub struct CallbackServer {
    cancel: Arc<AtomicBool>,
    wait_rx: Option<oneshot::Receiver<Option<CallbackResult>>>,
    local_addr: SocketAddr,
    _join: tokio::task::JoinHandle<()>,
}

impl CallbackServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn cancel_wait(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub async fn wait_for_code(mut self) -> Result<Option<CallbackResult>, OAuthError> {
        let rx = self
            .wait_rx
            .take()
            .ok_or_else(|| OAuthError::Other("callback wait already consumed".into()))?;
        match rx.await {
            Ok(value) => Ok(value),
            Err(_) => Ok(None),
        }
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug)]
pub struct CallbackServerConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub expected_state: Option<String>,
    pub success_message: String,
}

pub async fn start_callback_server(
    config: CallbackServerConfig,
) -> Result<CallbackServer, OAuthError> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|error| OAuthError::Other(format!("invalid callback address: {error}")))?;
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|error| OAuthError::Other(format!("failed to bind OAuth callback: {error}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|error| OAuthError::Other(format!("callback local_addr: {error}")))?;

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = oneshot::channel();
    let cancel_task = Arc::clone(&cancel);
    let join = tokio::spawn(async move {
        run_server(listener, config, cancel_task, tx).await;
    });

    Ok(CallbackServer {
        cancel,
        wait_rx: Some(rx),
        local_addr,
        _join: join,
    })
}

async fn run_server(
    listener: TcpListener,
    config: CallbackServerConfig,
    cancel: Arc<AtomicBool>,
    tx: oneshot::Sender<Option<CallbackResult>>,
) {
    let mut tx = Some(tx);
    loop {
        if cancel.load(Ordering::Acquire) {
            if let Some(tx) = tx.take() {
                let _ = tx.send(None);
            }
            return;
        }

        let accept = tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept());
        let Ok(Ok((stream, _))) = accept.await else {
            continue;
        };

        match handle_connection(stream, &config).await {
            HandleOutcome::Ignore => {}
            HandleOutcome::Settle(result) => {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(result);
                }
                // Keep serving briefly so the browser gets the response; then exit.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                return;
            }
        }
    }
}

enum HandleOutcome {
    Ignore,
    Settle(Option<CallbackResult>),
}

async fn handle_connection(mut stream: TcpStream, config: &CallbackServerConfig) -> HandleOutcome {
    let mut buf = vec![0_u8; 8192];
    let Ok(n) = stream.read(&mut buf).await else {
        return HandleOutcome::Ignore;
    };
    if n == 0 {
        return HandleOutcome::Ignore;
    }
    let request = String::from_utf8_lossy(&buf[..n]);
    let request_line = request.lines().next().unwrap_or("");
    let path_and_query = request_line.split_whitespace().nth(1).unwrap_or("/");

    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };

    if path != config.path {
        let body = oauth_error_html("Callback route not found.", None);
        let _ = write_response(&mut stream, 404, &body).await;
        return HandleOutcome::Ignore;
    }

    let params = query_map(query.unwrap_or(""));
    if let Some(expected) = &config.expected_state {
        let state = params.get("state").map(String::as_str).unwrap_or("");
        if state != expected {
            let body = oauth_error_html("State mismatch.", None);
            let _ = write_response(&mut stream, 400, &body).await;
            return HandleOutcome::Ignore;
        }
    }

    if let Some(error) = params.get("error") {
        let details = params
            .get("error_description")
            .map(String::as_str)
            .unwrap_or(error.as_str());
        let body = oauth_error_html(details, None);
        let _ = write_response(&mut stream, 400, &body).await;
        return HandleOutcome::Settle(None);
    }

    let Some(code) = params.get("code").cloned() else {
        let body = oauth_error_html("Missing authorization code.", None);
        let _ = write_response(&mut stream, 400, &body).await;
        return HandleOutcome::Ignore;
    };

    let body = oauth_success_html(&config.success_message);
    let _ = write_response(&mut stream, 200, &body).await;
    HandleOutcome::Settle(Some(CallbackResult {
        code,
        state: params.get("state").cloned(),
    }))
}

fn query_map(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = urlencoding_decode(key);
        let value = urlencoding_decode(value);
        map.insert(key, value);
    }
    map
}

fn urlencoding_decode(input: &str) -> String {
    // percent-encoding is already a dependency; use a tiny decoder for query values.
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &input[i + 1..i + 3];
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}
