//! OpenAI Codex responses transport.
//!
//! Ports oracle `openai-codex-responses.ts`:
//! - WebSocket transport with session-scoped SSE fallback cache (:60-68, :275-348)
//! - Session WS reuse (idle TTL 5m / max age 55m) with `previous_response_id` deltas (:1043-1457)
//! - `response.create` envelope on the WS path (oracle :1420)
//! - One retry on `websocket_connection_limit_reached` before stream start (oracle :283-316)
//! - zstd request compression on the SSE path (level 3)
//! - SSE parse shared with openai-responses via the incremental decoder
//!
//! Live network is never used in tests: sockets are behind [`WebSocketConnector`].

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::{sync::mpsc, time::Instant};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{
        AssistantMessage, AssistantMessageEvent, Content, Context, Message as PiMessage, Model,
        StopReason, StreamOptions, TextContent, ToolCall, Transport, Usage,
    },
};

use super::{common, openai_responses, transform_messages};

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_INSTRUCTIONS: &str = "You are a helpful assistant.";
const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;
const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
const SESSION_WEBSOCKET_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const SESSION_WEBSOCKET_MAX_AGE_MS: u64 = 55 * 60 * 1000;
const WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE: u16 = 1009;
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

static SSE_FALLBACK_SESSIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Session-owned WebSocket tasks (oracle `websocketSessionCache`).
static SESSION_WS_CACHE: LazyLock<Mutex<HashMap<String, SessionWsHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// WebSocket abstraction (test-injectable)
// ---------------------------------------------------------------------------

/// Wire-level WebSocket message used by both live and mock transports.
#[derive(Debug, Clone)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close { code: Option<u16>, reason: String },
}

#[async_trait]
pub trait WebSocketConn: Send {
    async fn send_text(&mut self, text: String) -> Result<(), String>;
    async fn next(&mut self) -> Result<Option<WsMessage>, String>;
    async fn close(&mut self) -> Result<(), String>;
}

#[async_trait]
pub trait WebSocketConnector: Send + Sync {
    async fn connect(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Box<dyn WebSocketConn>, String>;
}

/// Live connector via `tokio-tungstenite`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TungsteniteConnector;

#[async_trait]
impl WebSocketConnector for TungsteniteConnector {
    async fn connect(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Box<dyn WebSocketConn>, String> {
        let mut request = url
            .into_client_request()
            .map_err(|error| format!("websocket request: {error}"))?;
        {
            let hdrs = request.headers_mut();
            for (name, value) in headers {
                if let (Ok(n), Ok(v)) = (
                    name.parse::<tokio_tungstenite::tungstenite::http::header::HeaderName>(),
                    HeaderValue::from_str(value),
                ) {
                    hdrs.insert(n, v);
                }
            }
        }
        let (socket, _) = connect_async(request)
            .await
            .map_err(|error| format!("websocket connect: {error}"))?;
        Ok(Box::new(TungsteniteConn { socket }))
    }
}

struct TungsteniteConn {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[async_trait]
impl WebSocketConn for TungsteniteConn {
    async fn send_text(&mut self, text: String) -> Result<(), String> {
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| format!("websocket send: {error}"))
    }

    async fn next(&mut self) -> Result<Option<WsMessage>, String> {
        loop {
            match self.socket.next().await {
                None => return Ok(None),
                Some(Err(error)) => return Err(format!("websocket read: {error}")),
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(WsMessage::Text(text.to_string())));
                }
                Some(Ok(Message::Binary(bytes))) => {
                    return Ok(Some(WsMessage::Binary(bytes.to_vec())));
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = self.socket.send(Message::Pong(data.clone())).await;
                    return Ok(Some(WsMessage::Ping(data.to_vec())));
                }
                Some(Ok(Message::Pong(data))) => {
                    return Ok(Some(WsMessage::Pong(data.to_vec())));
                }
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = match frame {
                        Some(frame) => {
                            let code: u16 = frame.code.into();
                            (Some(code), frame.reason.to_string())
                        }
                        None => (None, String::new()),
                    };
                    return Ok(Some(WsMessage::Close { code, reason }));
                }
                Some(Ok(Message::Frame(_))) => continue,
            }
        }
    }

    async fn close(&mut self) -> Result<(), String> {
        self.socket
            .close(None)
            .await
            .map_err(|error| format!("websocket close: {error}"))
    }
}

// ---------------------------------------------------------------------------
// Request body / headers
// ---------------------------------------------------------------------------

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = openai_responses::build_request_body(model, context, options);
    if let Some(input) = body["input"].as_array_mut()
        && input.first().and_then(|item| item["role"].as_str()) == Some("system")
    {
        input.remove(0);
    }
    body.as_object_mut()
        .expect("request body is an object")
        .remove("max_output_tokens");
    body["instructions"] = Value::String(
        context
            .system_prompt
            .clone()
            .unwrap_or_else(|| CODEX_INSTRUCTIONS.into()),
    );
    body["text"] = json!({"verbosity":"low"});
    body["include"] = json!(["reasoning.encrypted_content"]);
    body["tool_choice"] = Value::String("auto".into());
    body["parallel_tool_calls"] = Value::Bool(true);
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            tool["strict"] = Value::Null;
        }
    }
    if let Some(session_id) = &options.session_id {
        body["prompt_cache_key"] = Value::String(session_id.clone());
    }
    body
}

/// Oracle :1420 — WS frames are `{ type: "response.create", ...requestBody }`.
pub fn wrap_response_create_envelope(body: &Value) -> Value {
    let mut envelope = Map::new();
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            envelope.insert(key.clone(), value.clone());
        }
    }
    // Force type last so body cannot override the envelope kind.
    envelope.insert("type".into(), Value::String("response.create".into()));
    Value::Object(envelope)
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    build_sse_headers(model, options, false)
}

fn build_sse_headers(
    model: &Model,
    options: &StreamOptions,
    with_zstd: bool,
) -> Vec<(String, String)> {
    let mut headers = openai_responses::build_headers(model, options);
    headers.push(("originator".into(), "pi".into()));
    headers.push(("openai-beta".into(), "responses=experimental".into()));
    headers.push(("accept".into(), "text/event-stream".into()));
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(account) = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("accountId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| options.api_key.as_deref().and_then(extract_account_id))
    {
        headers.push(("chatgpt-account-id".into(), account));
    }
    if let Some(session_id) = &options.session_id {
        headers.push(("session-id".into(), session_id.clone()));
        headers.push(("x-client-request-id".into(), session_id.clone()));
    }
    if with_zstd {
        headers.push(("content-encoding".into(), "zstd".into()));
    }
    headers
}

fn build_websocket_headers(
    model: &Model,
    options: &StreamOptions,
    request_id: &str,
) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    if let Some(key) = &options.api_key {
        headers.push(("authorization".into(), format!("Bearer {key}")));
    }
    if let Some(account) = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("accountId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| options.api_key.as_deref().and_then(extract_account_id))
    {
        headers.push(("chatgpt-account-id".into(), account));
    }
    headers.push(("originator".into(), "pi".into()));
    headers.push((
        "openai-beta".into(),
        OPENAI_BETA_RESPONSES_WEBSOCKETS.into(),
    ));
    headers.push(("x-client-request-id".into(), request_id.into()));
    headers.push(("session-id".into(), request_id.into()));
    headers
}

pub fn parse_stream_events<I, B>(
    chunks: I,
    model: &Model,
) -> common::ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    common::decode_sse_chunks(chunks, super::incremental::decoder(model))
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested, no network)
// ---------------------------------------------------------------------------

/// Compress JSON request body with zstd level 3 (oracle REQUEST_COMPRESSION_ZSTD_LEVEL).
pub fn compress_request_body_zstd(body_json: &[u8]) -> Option<Vec<u8>> {
    zstd::bulk::compress(body_json, REQUEST_COMPRESSION_ZSTD_LEVEL).ok()
}

pub fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url.trim()
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_owned()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

pub fn resolve_codex_websocket_url(base_url: &str) -> String {
    let http = resolve_codex_url(base_url);
    if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http
    }
}

pub fn is_websocket_sse_fallback_active(session_id: Option<&str>) -> bool {
    session_id
        .map(|id| SSE_FALLBACK_SESSIONS.lock().contains(id))
        .unwrap_or(false)
}

pub fn record_websocket_sse_fallback(session_id: Option<&str>) {
    if let Some(id) = session_id {
        SSE_FALLBACK_SESSIONS.lock().insert(id.to_owned());
    }
}

pub fn clear_websocket_sse_fallback(session_id: Option<&str>) {
    let mut guard = SSE_FALLBACK_SESSIONS.lock();
    if let Some(id) = session_id {
        guard.remove(id);
    } else {
        guard.clear();
    }
}

pub fn should_fallback_to_sse(error: &str, websocket_started: bool) -> bool {
    if websocket_started {
        return false;
    }
    error.contains(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE)
        || error.contains(&WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE.to_string())
        || error.contains("websocket")
        || error.contains("WebSocket")
        || error.contains("connection")
}

pub fn is_connection_limit_error(error: &str) -> bool {
    error.contains(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE)
}

pub fn is_session_ws_expired(created_at: Instant, now: Instant) -> bool {
    now.duration_since(created_at) >= Duration::from_millis(SESSION_WEBSOCKET_MAX_AGE_MS)
}

pub fn is_session_ws_idle_expired(last_used: Instant, now: Instant) -> bool {
    now.duration_since(last_used) >= Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS)
}

/// Build a continuation body with `previous_response_id` + input delta when possible
/// (oracle `buildCachedWebSocketRequestBody` / `getCachedWebSocketInputDelta`).
pub fn build_cached_websocket_request_body(body: &Value, continuation: &WsContinuation) -> Value {
    let Some(delta) = cached_input_delta(body, continuation) else {
        return body.clone();
    };
    let mut out = body.clone();
    out["previous_response_id"] = Value::String(continuation.last_response_id.clone());
    out["input"] = Value::Array(delta);
    out
}

fn request_body_without_input(body: &Value) -> Value {
    let mut stripped = body.clone();
    if let Some(obj) = stripped.as_object_mut() {
        obj.remove("input");
        obj.remove("previous_response_id");
    }
    stripped
}

fn cached_input_delta(body: &Value, continuation: &WsContinuation) -> Option<Vec<Value>> {
    if request_body_without_input(body)
        != request_body_without_input(&continuation.last_request_body)
    {
        return None;
    }
    let current = body.get("input").and_then(Value::as_array)?.clone();
    let mut baseline = continuation
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current.len() < baseline.len() {
        return None;
    }
    if current[..baseline.len()] != baseline[..] {
        return None;
    }
    Some(current[baseline.len()..].to_vec())
}

fn extract_account_id(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let _sig = parts.next()?;
    let padded = match payload_b64.len() % 4 {
        2 => format!("{payload_b64}=="),
        3 => format!("{payload_b64}="),
        _ => payload_b64.to_owned(),
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let bytes = URL_SAFE_NO_PAD
        .decode(padded.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&padded))
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .pointer(&format!("/{JWT_CLAIM_PATH}/chatgpt_account_id"))
        .or_else(|| value.pointer("/chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Session WebSocket cache (session task owns the socket)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WsContinuation {
    pub last_request_body: Value,
    pub last_response_id: String,
    pub last_response_items: Vec<Value>,
}

struct SessionCommand {
    body: Value,
    use_cached_context: bool,
    /// Incremental turn events: frames first, then exactly one Done or Error.
    events: mpsc::Sender<TurnEvent>,
}

enum TurnEvent {
    Frame(String),
    Done {
        response_id: Option<String>,
        reused: bool,
    },
    Error(String),
}

/// Successful turn payload: response id + converted response items for continuation.
struct TurnOutcome {
    response_id: Option<String>,
    response_items: Vec<Value>,
}

#[derive(Clone)]
struct SessionWsHandle {
    tx: mpsc::Sender<SessionCommand>,
    created_at: Instant,
    last_used: Instant,
    /// Oracle `busy`: true while a turn is in flight on the cached socket.
    busy: Arc<std::sync::atomic::AtomicBool>,
}

/// Drop every cached session socket (tests).
pub fn close_session_websockets(session_id: Option<&str>) {
    let mut guard = SESSION_WS_CACHE.lock();
    if let Some(id) = session_id {
        guard.remove(id);
    } else {
        guard.clear();
    }
}

fn session_handle_is_live(handle: &SessionWsHandle, now: Instant) -> bool {
    !is_session_ws_expired(handle.created_at, now)
        && !is_session_ws_idle_expired(handle.last_used, now)
        && !handle.tx.is_closed()
}

/// Convert a completed assistant turn into response-input items (oracle
/// `convertResponsesMessages(..., {includeSystemPrompt:false}).filter(type !== function_call_output)`).
fn last_response_items_from_output(model: &Model, content: Vec<Content>) -> Vec<Value> {
    let assistant = AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    };
    let ctx = Context {
        messages: vec![PiMessage::Assistant(assistant)],
        tools: vec![],
        system_prompt: None,
    };
    transform_messages::responses_input(&ctx)
        .into_iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("function_call_output"))
        .collect()
}

/// Reconstruct assistant content from `response.output` items of a completed frame.
fn content_from_response_output(output: &[Value]) -> Vec<Content> {
    let mut content = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                            if text.is_empty() {
                                continue;
                            }
                            let text_signature = item
                                .get("id")
                                .and_then(Value::as_str)
                                .map(|id| {
                                    serde_json::to_string(&crate::types::TextSignatureV1 {
                                        v: 1,
                                        id: id.to_owned(),
                                        phase: None,
                                    })
                                    .unwrap_or_default()
                                })
                                .filter(|s| !s.is_empty());
                            content.push(Content::Text(TextContent {
                                text: crate::shared_text::SharedText::from_str(text),
                                text_signature,
                            }));
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let item_id = item.get("id").and_then(Value::as_str);
                let id = match item_id {
                    Some(item_id) if !item_id.is_empty() => format!("{call_id}|{item_id}"),
                    _ => call_id.to_owned(),
                };
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Map<String, Value>>(raw).ok())
                    .unwrap_or_default();
                content.push(Content::ToolCall(ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature: None,
                }));
            }
            Some("reasoning") => {
                // Persist the whole item as thinking_signature so responses_input can re-emit it.
                if let Ok(sig) = serde_json::to_string(item) {
                    content.push(Content::Thinking(crate::types::ThinkingContent {
                        thinking: crate::shared_text::SharedText::default(),
                        thinking_signature: Some(sig),
                        redacted: None,
                    }));
                }
            }
            _ => {}
        }
    }
    content
}

async fn acquire_session_handle(
    session_id: &str,
    url: &str,
    headers: &[(String, String)],
    connector: Arc<dyn WebSocketConnector>,
) -> Result<SessionWsHandle, String> {
    let now = Instant::now();
    let busy_cached = {
        let mut guard = SESSION_WS_CACHE.lock();
        if let Some(existing) = guard.get(session_id).cloned() {
            if session_handle_is_live(&existing, now) {
                // Oracle :1091 — busy cached socket → ephemeral (no reuse).
                if existing.busy.load(std::sync::atomic::Ordering::SeqCst) {
                    true
                } else {
                    if let Some(entry) = guard.get_mut(session_id) {
                        entry.last_used = now;
                        entry.busy.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    existing
                        .busy
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return Ok(existing);
                }
            } else {
                guard.remove(session_id);
                false
            }
        } else {
            false
        }
    };
    if busy_cached {
        return connect_ephemeral_session_handle(url, headers, connector).await;
    }

    let socket = connector.connect(url, headers).await?;
    let (tx, rx) = mpsc::channel::<SessionCommand>(8);
    let created_at = Instant::now();
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    tokio::spawn(session_ws_task(
        session_id.to_owned(),
        socket,
        rx,
        created_at,
        busy.clone(),
    ));
    let handle = SessionWsHandle {
        tx,
        created_at,
        last_used: created_at,
        busy,
    };
    SESSION_WS_CACHE
        .lock()
        .insert(session_id.to_owned(), handle.clone());
    Ok(handle)
}

/// Ephemeral one-shot session task (oracle busy-path: connect without caching reuse).
async fn connect_ephemeral_session_handle(
    url: &str,
    headers: &[(String, String)],
    connector: Arc<dyn WebSocketConnector>,
) -> Result<SessionWsHandle, String> {
    let socket = connector.connect(url, headers).await?;
    let (tx, rx) = mpsc::channel::<SessionCommand>(8);
    let created_at = Instant::now();
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // Empty session_id → task does not register in SESSION_WS_CACHE.
    tokio::spawn(session_ws_task(
        String::new(),
        socket,
        rx,
        created_at,
        busy.clone(),
    ));
    Ok(SessionWsHandle {
        tx,
        created_at,
        last_used: created_at,
        busy,
    })
}

async fn session_ws_task(
    session_id: String,
    mut socket: Box<dyn WebSocketConn>,
    mut rx: mpsc::Receiver<SessionCommand>,
    created_at: Instant,
    busy: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut continuation: Option<WsContinuation> = None;
    let mut last_used = Instant::now();
    // Oracle scheduleSessionWebSocketExpiry: proactive close after idle TTL.
    let idle_ttl = Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS);
    let max_age = Duration::from_millis(SESSION_WEBSOCKET_MAX_AGE_MS);

    loop {
        let now = Instant::now();
        let idle_deadline = last_used + idle_ttl;
        let age_deadline = created_at + max_age;
        let next_deadline = idle_deadline.min(age_deadline);
        let wait = next_deadline.saturating_duration_since(now);

        let cmd = tokio::select! {
            biased;
            maybe = rx.recv() => maybe,
            _ = tokio::time::sleep(wait) => {
                // Proactive eviction: only when not mid-turn (oracle idleTimer skips if busy).
                if busy.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let now = Instant::now();
                if is_session_ws_expired(created_at, now) || is_session_ws_idle_expired(last_used, now)
                {
                    break;
                }
                continue;
            }
        };

        let Some(cmd) = cmd else {
            break;
        };

        let now = Instant::now();
        if is_session_ws_expired(created_at, now) || is_session_ws_idle_expired(last_used, now) {
            let _ = cmd
                .events
                .send(TurnEvent::Error("websocket session expired".into()))
                .await;
            while let Ok(queued) = rx.try_recv() {
                let _ = queued
                    .events
                    .send(TurnEvent::Error("websocket session expired".into()))
                    .await;
            }
            break;
        }

        busy.store(true, std::sync::atomic::Ordering::SeqCst);

        let full_body = cmd.body;
        let request_body = if cmd.use_cached_context {
            if let Some(cont) = continuation.as_ref() {
                build_cached_websocket_request_body(&full_body, cont)
            } else {
                full_body.clone()
            }
        } else {
            full_body.clone()
        };
        let reused = continuation.is_some()
            && request_body
                .get("previous_response_id")
                .and_then(Value::as_str)
                .is_some();

        let envelope = wrap_response_create_envelope(&request_body);
        let payload = match serde_json::to_string(&envelope) {
            Ok(s) => s,
            Err(error) => {
                let _ = cmd.events.send(TurnEvent::Error(error.to_string())).await;
                busy.store(false, std::sync::atomic::Ordering::SeqCst);
                continue;
            }
        };

        if let Err(error) = socket.send_text(payload).await {
            let _ = cmd.events.send(TurnEvent::Error(error)).await;
            while let Ok(queued) = rx.try_recv() {
                let _ = queued
                    .events
                    .send(TurnEvent::Error("websocket session closed".into()))
                    .await;
            }
            break;
        }

        match stream_ws_turn(&mut *socket, &cmd.events).await {
            Ok(outcome) => {
                last_used = Instant::now();
                if let Some(rid) = outcome.response_id.clone() {
                    if cmd.use_cached_context {
                        continuation = Some(WsContinuation {
                            last_request_body: full_body,
                            last_response_id: rid,
                            last_response_items: outcome.response_items,
                        });
                    } else {
                        continuation = None;
                    }
                } else {
                    continuation = None;
                }
                if !session_id.is_empty()
                    && let Some(entry) = SESSION_WS_CACHE.lock().get_mut(&session_id)
                {
                    entry.last_used = last_used;
                }
                let _ = cmd
                    .events
                    .send(TurnEvent::Done {
                        response_id: outcome.response_id,
                        reused,
                    })
                    .await;
                busy.store(false, std::sync::atomic::Ordering::SeqCst);
                // Busy-path sockets are one-shot and intentionally never cached.
                if session_id.is_empty() {
                    break;
                }
            }
            Err(error) => {
                // Socket is no longer reusable after a mid-turn failure.
                let _ = cmd.events.send(TurnEvent::Error(error)).await;
                while let Ok(queued) = rx.try_recv() {
                    let _ = queued
                        .events
                        .send(TurnEvent::Error("websocket session closed".into()))
                        .await;
                }
                break;
            }
        }
    }

    let _ = socket.close().await;
    if !session_id.is_empty() {
        let mut guard = SESSION_WS_CACHE.lock();
        if guard
            .get(&session_id)
            .is_some_and(|h| h.created_at == created_at)
        {
            guard.remove(&session_id);
        }
    }
}

/// Read one WS turn, forwarding each frame immediately.
/// Returns response_id + converted last_response_items for continuation baseline.
async fn stream_ws_turn(
    socket: &mut dyn WebSocketConn,
    events: &mpsc::Sender<TurnEvent>,
) -> Result<TurnOutcome, String> {
    let mut response_id = None;
    let mut response_items = Vec::new();

    loop {
        match socket.next().await? {
            None => {
                return Err("WebSocket stream closed before response.completed".into());
            }
            Some(WsMessage::Text(text)) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    if let Some(id) = parsed
                        .pointer("/response/id")
                        .and_then(Value::as_str)
                        .or_else(|| parsed.get("id").and_then(Value::as_str))
                    {
                        response_id = Some(id.to_owned());
                    }
                    match parsed.get("type").and_then(Value::as_str) {
                        Some("error") => {
                            let code = parsed
                                .get("code")
                                .and_then(Value::as_str)
                                .or_else(|| parsed.pointer("/error/code").and_then(Value::as_str))
                                .unwrap_or("");
                            let message = parsed
                                .get("message")
                                .and_then(Value::as_str)
                                .or_else(|| {
                                    parsed.pointer("/error/message").and_then(Value::as_str)
                                })
                                .unwrap_or("Codex error");
                            if code == WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE
                                || message.contains(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE)
                            {
                                return Err(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE.into());
                            }
                            return Err(format!("Codex error: {message}"));
                        }
                        Some("response.failed") => {
                            let code = parsed
                                .pointer("/response/error/code")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let message = parsed
                                .pointer("/response/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or("Codex response failed");
                            if code == WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE {
                                return Err(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE.into());
                            }
                            return Err(message.into());
                        }
                        Some("response.completed" | "response.done" | "response.incomplete") => {
                            // Capture response.output → last_response_items via responses_input.
                            if let Some(output) =
                                parsed.pointer("/response/output").and_then(Value::as_array)
                            {
                                let model = Model {
                                    id: parsed
                                        .pointer("/response/model")
                                        .and_then(Value::as_str)
                                        .unwrap_or("gpt-test")
                                        .to_owned(),
                                    name: String::new(),
                                    api: crate::types::Api::from("openai-codex-responses"),
                                    provider: "openai-codex".into(),
                                    base_url: String::new(),
                                    reasoning: false,
                                    thinking_level_map: None,
                                    input: vec![],
                                    cost: crate::types::ModelCost::default(),
                                    context_window: 0,
                                    max_tokens: 0,
                                    headers: None,
                                    compat: None,
                                };
                                let content = content_from_response_output(output);
                                response_items = last_response_items_from_output(&model, content);
                            }
                            let _ = events.send(TurnEvent::Frame(text)).await;
                            return Ok(TurnOutcome {
                                response_id,
                                response_items,
                            });
                        }
                        _ => {
                            let _ = events.send(TurnEvent::Frame(text)).await;
                        }
                    }
                } else {
                    let _ = events.send(TurnEvent::Frame(text)).await;
                }
            }
            Some(WsMessage::Binary(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let _ = events.send(TurnEvent::Frame(text)).await;
            }
            Some(WsMessage::Close { code, reason }) => {
                if code == Some(WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE) {
                    return Err(format!(
                        "websocket close {WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE}: {reason}"
                    ));
                }
                if reason.contains(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE) {
                    return Err(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE.into());
                }
                return Err(format!(
                    "WebSocket closed{}{}",
                    code.map(|c| format!(" {c}")).unwrap_or_default(),
                    if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" {reason}")
                    }
                ));
            }
            Some(WsMessage::Ping(_) | WsMessage::Pong(_)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

pub fn stream_with_client(
    model: Model,
    context: Context,
    options: StreamOptions,
    client: Arc<dyn StreamHttpClient>,
) -> AssistantMessageEventStream {
    let body = build_request_body(&model, &context, &options);
    let body_json = serde_json::to_vec(&body).unwrap_or_default();
    let compressed = compress_request_body_zstd(&body_json);
    let use_zstd = compressed.is_some();
    let headers = build_sse_headers(&model, &options, use_zstd);
    let url = resolve_codex_url(&model.base_url);

    let stream = AssistantMessageEventStream::new();
    let producer = stream.clone();
    let mut decoder = super::incremental::decoder(&model);
    tokio::spawn(async move {
        let result: common::ApiResult<()> = async {
            let bytes = compressed.unwrap_or(body_json);
            let mut response = client
                .post_bytes(&url, &headers, &bytes)
                .await
                .map_err(|error| error.to_string())?;
            let mut frames = crate::sse::SseParser::default();
            for event in decoder.initial_events() {
                producer.push(event);
            }
            while let Some(chunk) = response.next().await {
                let chunk = chunk.map_err(|error| error.to_string())?;
                for frame in frames.push(&chunk) {
                    for event in decoder.push_frame(&frame)? {
                        producer.push(event);
                    }
                }
            }
            for frame in frames.finish() {
                for event in decoder.push_frame(&frame)? {
                    producer.push(event);
                }
            }
            for event in decoder.finish()? {
                producer.push(event);
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let mut message = common::empty_message(&model);
            message.stop_reason = StopReason::Error;
            message.error_message = Some(error);
            producer.push(AssistantMessageEvent::Error {
                reason: StopReason::Error,
                error: message,
            });
        }
        producer.end(None);
    });
    stream
}

/// Live path: try WebSocket (unless transport=sse or session is in SSE-fallback
/// cache), then fall back to zstd-compressed SSE.
pub fn stream(
    model: Model,
    context: Context,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    stream_with_ws_connector(
        model,
        context,
        options,
        Arc::new(TungsteniteConnector),
        None,
    )
}

/// Injectable path used by tests: optional WS connector + optional SSE client.
pub fn stream_with_ws_connector(
    model: Model,
    context: Context,
    options: StreamOptions,
    connector: Arc<dyn WebSocketConnector>,
    sse_client: Option<Arc<dyn StreamHttpClient>>,
) -> AssistantMessageEventStream {
    let transport = options.transport.unwrap_or(Transport::Auto);
    let session_id = options.session_id.clone();
    let ws_disabled = matches!(transport, Transport::Sse)
        || is_websocket_sse_fallback_active(session_id.as_deref());

    if matches!(
        transport,
        Transport::Websocket | Transport::WebsocketCached | Transport::Auto
    ) && !ws_disabled
    {
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        tokio::spawn(async move {
            let body = build_request_body(&model, &context, &options);
            let request_id = session_id
                .clone()
                .unwrap_or_else(|| format!("codex_{}", common::now_ms()));
            let ws_url = resolve_codex_websocket_url(&model.base_url);
            let ws_headers = build_websocket_headers(&model, &options, &request_id);
            let use_cached_context =
                matches!(transport, Transport::WebsocketCached | Transport::Auto);

            let mut retried_connection_limit = false;
            loop {
                let mut websocket_started = false;
                match process_websocket_stream(
                    &ws_url,
                    &ws_headers,
                    &body,
                    &model,
                    session_id.as_deref(),
                    use_cached_context,
                    connector.clone(),
                    &mut websocket_started,
                    producer.clone(),
                )
                .await
                {
                    Ok(()) => {
                        producer.end(None);
                        return;
                    }
                    Err(error) => {
                        let connection_limit_before_start =
                            !websocket_started && is_connection_limit_error(&error);
                        if connection_limit_before_start && !retried_connection_limit {
                            retried_connection_limit = true;
                            // Drop any half-open session entry before retrying.
                            close_session_websockets(session_id.as_deref());
                            continue;
                        }
                        if should_fallback_to_sse(&error, websocket_started) {
                            record_websocket_sse_fallback(session_id.as_deref());
                            break;
                        }
                        let mut message = common::empty_message(&model);
                        message.stop_reason = StopReason::Error;
                        message.error_message = Some(error);
                        producer.push(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: message,
                        });
                        producer.end(None);
                        return;
                    }
                }
            }

            // SSE fallback with zstd compression.
            match sse_client {
                Some(client) => {
                    let mut sse_stream = stream_with_client(model, context, options, client);
                    while let Some(event) = sse_stream.next().await {
                        producer.push(event);
                    }
                    producer.end(None);
                }
                None => match ReqwestStreamHttpClient::new() {
                    Ok(client) => {
                        let mut sse_stream =
                            stream_with_client(model, context, options, Arc::new(client));
                        while let Some(event) = sse_stream.next().await {
                            producer.push(event);
                        }
                        producer.end(None);
                    }
                    Err(error) => {
                        let mut message = common::empty_message(&model);
                        message.stop_reason = StopReason::Error;
                        message.error_message = Some(error.to_string());
                        producer.push(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: message,
                        });
                        producer.end(None);
                    }
                },
            }
        });
        return stream;
    }

    match sse_client {
        Some(client) => stream_with_client(model, context, options, client),
        None => match ReqwestStreamHttpClient::new() {
            Ok(client) => stream_with_client(model, context, options, Arc::new(client)),
            Err(error) => {
                let stream = AssistantMessageEventStream::new();
                let mut message = common::empty_message(&model);
                message.stop_reason = StopReason::Error;
                message.error_message = Some(error.to_string());
                stream.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: message,
                });
                stream
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_websocket_stream(
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    model: &Model,
    session_id: Option<&str>,
    use_cached_context: bool,
    connector: Arc<dyn WebSocketConnector>,
    websocket_started: &mut bool,
    producer: AssistantMessageEventStream,
) -> common::ApiResult<()> {
    let mut decoder = super::incremental::decoder(model);

    if let Some(session_id) = session_id {
        let handle = acquire_session_handle(session_id, url, headers, connector).await?;
        let (events_tx, mut events_rx) = mpsc::channel::<TurnEvent>(64);
        handle
            .tx
            .send(SessionCommand {
                body: body.clone(),
                use_cached_context,
                events: events_tx,
            })
            .await
            .map_err(|_| "websocket session closed".to_owned())?;

        let mut saw_terminal = false;
        while let Some(event) = events_rx.recv().await {
            match event {
                TurnEvent::Frame(frame) => {
                    *websocket_started = true;
                    for decoded in decoder.push_frame(&frame)? {
                        producer.push(decoded);
                    }
                }
                TurnEvent::Done {
                    response_id,
                    reused,
                } => {
                    let _ = (response_id, reused);
                    saw_terminal = true;
                    break;
                }
                TurnEvent::Error(error) => return Err(error),
            }
        }
        // Channel closed without Done/Error → orphaned turn is an error.
        // `busy` is owned solely by session_ws_task: it releases on turn end
        // and evicts the cache entry on death — a consumer-side store here
        // would clobber a subsequent acquirer's claim (busy-flag race).
        if !saw_terminal {
            return Err("websocket session closed before turn completed".into());
        }
    } else {
        // Ephemeral connection (no session id) — no reuse; still stream frames.
        let mut socket = connector.connect(url, headers).await?;
        let envelope = wrap_response_create_envelope(body);
        let payload = serde_json::to_string(&envelope).map_err(|error| error.to_string())?;
        socket.send_text(payload).await?;

        let (events_tx, mut events_rx) = mpsc::channel::<TurnEvent>(64);
        let read = async {
            let result = stream_ws_turn(&mut *socket, &events_tx).await;
            match result {
                Ok(outcome) => {
                    let _ = events_tx
                        .send(TurnEvent::Done {
                            response_id: outcome.response_id,
                            reused: false,
                        })
                        .await;
                }
                Err(error) => {
                    let _ = events_tx.send(TurnEvent::Error(error)).await;
                }
            }
            let _ = socket.close().await;
        };
        let consume = async {
            let mut saw_terminal = false;
            while let Some(event) = events_rx.recv().await {
                match event {
                    TurnEvent::Frame(frame) => {
                        *websocket_started = true;
                        for decoded in decoder.push_frame(&frame)? {
                            producer.push(decoded);
                        }
                    }
                    TurnEvent::Done {
                        response_id,
                        reused,
                    } => {
                        let _ = (response_id, reused);
                        saw_terminal = true;
                        break;
                    }
                    TurnEvent::Error(error) => return Err(error),
                }
            }
            if !saw_terminal {
                return Err("websocket session closed before turn completed".into());
            }
            Ok::<(), String>(())
        };
        // Drive reader + consumer concurrently so frames flush immediately.
        tokio::pin!(read);
        tokio::pin!(consume);
        tokio::select! {
            _ = &mut read => {
                consume.await?;
            }
            result = &mut consume => {
                result?;
                read.await;
            }
        }
    }

    for event in decoder.finish()? {
        producer.push(event);
    }
    Ok(())
}

pub fn stream_simple(
    model: Model,
    context: Context,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    stream(model, context, options)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        http::{HttpByteStream, HttpError, HttpFuture, StreamHttpClient},
        types::{Api, Message, ModelCost, ModelInput, UserContent, UserMessage},
    };
    use futures_util::stream;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Mutex as AsyncMutex;

    fn test_model() -> Model {
        Model {
            id: "gpt-test".into(),
            name: "Test".into(),
            api: Api::from("openai-codex-responses"),
            provider: "openai-codex".into(),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            compat: None,
        }
    }

    fn test_context(text: &str) -> Context {
        Context {
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text(text.into()),
                timestamp: 0,
            })],
            tools: vec![],
            system_prompt: None,
        }
    }

    fn completed_frame(id: &str, text: &str) -> String {
        json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "status": "completed",
                "model": "gpt-test",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "id": format!("msg_{id}"),
                    "content": [{"type": "output_text", "text": text}]
                }],
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1
                }
            }
        })
        .to_string()
    }

    /// Context that continues a prior turn: prior user + assistant reply + new user.
    /// Assistant text_signature matches completed_frame's message id so
    /// `responses_input` baseline equals lastInput + lastResponseItems.
    fn continued_context(
        prior_user: &str,
        assistant_text: &str,
        response_id: &str,
        next_user: &str,
    ) -> Context {
        let text_signature = serde_json::to_string(&crate::types::TextSignatureV1 {
            v: 1,
            id: format!("msg_{response_id}"),
            phase: None,
        })
        .unwrap();
        Context {
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text(prior_user.into()),
                    timestamp: 0,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![Content::Text(TextContent {
                        text: crate::shared_text::SharedText::from_str(assistant_text),
                        text_signature: Some(text_signature),
                    })],
                    api: Api::from("openai-codex-responses"),
                    provider: "openai-codex".into(),
                    model: "gpt-test".into(),
                    response_model: None,
                    response_id: Some(response_id.into()),
                    diagnostics: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text(next_user.into()),
                    timestamp: 1,
                }),
            ],
            tools: vec![],
            system_prompt: None,
        }
    }

    // ----- mock WS -----

    #[derive(Clone, Default)]
    struct MockWsShared {
        /// Queues of inbound messages, one queue consumed per connection.
        inbound: Arc<AsyncMutex<Vec<Vec<WsMessage>>>>,
        /// Fail next N connects with connection-limit.
        fail_connects: Arc<AtomicUsize>,
        /// After fail_connects exhausted, optionally fail every remaining connect.
        always_fail_connect: Arc<AtomicBool>,
        connect_count: Arc<AtomicUsize>,
        sent: Arc<Mutex<Vec<String>>>,
        /// Close-with-limit on the Nth connection's first read (1-based; 0 = never).
        close_limit_on_conn: Arc<AtomicUsize>,
    }

    struct MockWsConnector {
        shared: MockWsShared,
    }

    struct MockWsConn {
        shared: MockWsShared,
        inbound: Vec<WsMessage>,
        conn_index: usize,
        closed: bool,
    }

    #[async_trait]
    impl WebSocketConnector for MockWsConnector {
        async fn connect(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<Box<dyn WebSocketConn>, String> {
            let n = self.shared.connect_count.fetch_add(1, Ordering::SeqCst) + 1;
            let remaining = self.shared.fail_connects.load(Ordering::SeqCst);
            if remaining > 0 {
                self.shared.fail_connects.fetch_sub(1, Ordering::SeqCst);
                return Err(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE.into());
            }
            if self.shared.always_fail_connect.load(Ordering::SeqCst) {
                return Err(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE.into());
            }
            let mut guard = self.shared.inbound.lock().await;
            let inbound = if guard.is_empty() {
                vec![WsMessage::Text(completed_frame(&format!("resp-{n}"), "ok"))]
            } else {
                guard.remove(0)
            };
            Ok(Box::new(MockWsConn {
                shared: self.shared.clone(),
                inbound,
                conn_index: n,
                closed: false,
            }))
        }
    }

    #[async_trait]
    impl WebSocketConn for MockWsConn {
        async fn send_text(&mut self, text: String) -> Result<(), String> {
            if self.closed {
                return Err("socket closed".into());
            }
            self.shared.sent.lock().push(text);
            Ok(())
        }

        async fn next(&mut self) -> Result<Option<WsMessage>, String> {
            if self.closed {
                return Ok(None);
            }
            let close_on = self.shared.close_limit_on_conn.load(Ordering::SeqCst);
            if close_on == self.conn_index {
                self.shared.close_limit_on_conn.store(0, Ordering::SeqCst);
                return Ok(Some(WsMessage::Close {
                    code: Some(1013),
                    reason: WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE.into(),
                }));
            }
            if self.inbound.is_empty() {
                return Ok(None);
            }
            Ok(Some(self.inbound.remove(0)))
        }

        async fn close(&mut self) -> Result<(), String> {
            self.closed = true;
            Ok(())
        }
    }

    #[derive(Clone)]
    struct RecordingHttp {
        hits: Arc<AtomicUsize>,
        body: Vec<u8>,
    }

    impl StreamHttpClient for RecordingHttp {
        fn post_sse<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
        ) -> HttpFuture<'a> {
            self.response()
        }

        fn post_json_stream<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
        ) -> HttpFuture<'a> {
            self.response()
        }

        fn post_bytes<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a [u8],
        ) -> HttpFuture<'a> {
            self.response()
        }
    }

    impl RecordingHttp {
        fn response(&self) -> HttpFuture<'_> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let chunks = self.body.clone();
            Box::pin(async move {
                let body: HttpByteStream = Box::pin(stream::iter(vec![Ok::<_, HttpError>(chunks)]));
                Ok(body)
            })
        }
    }

    // ----- pure helpers -----

    #[test]
    fn zstd_roundtrip_compresses() {
        let json = br#"{"model":"test","input":[{"role":"user","content":"hi"}]}"#;
        let compressed = compress_request_body_zstd(json).expect("compress");
        assert!(compressed.len() < json.len() + 64);
        let decompressed = zstd::bulk::decompress(&compressed, 64 * 1024).unwrap();
        assert_eq!(decompressed, json);
    }

    #[test]
    fn resolve_urls() {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_websocket_url("https://chatgpt.com/backend-api"),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn sse_fallback_session_cache() {
        let id = format!("unit-sse-{}", common::now_ms());
        assert!(!is_websocket_sse_fallback_active(Some(&id)));
        record_websocket_sse_fallback(Some(&id));
        assert!(is_websocket_sse_fallback_active(Some(&id)));
        assert!(!is_websocket_sse_fallback_active(Some("s2-never-recorded")));
        clear_websocket_sse_fallback(Some(&id));
        assert!(!is_websocket_sse_fallback_active(Some(&id)));
    }

    #[test]
    fn fallback_decision_before_start() {
        assert!(should_fallback_to_sse("websocket connect failed", false));
        assert!(should_fallback_to_sse(
            WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE,
            false
        ));
        assert!(!should_fallback_to_sse("protocol error", true));
    }

    #[test]
    fn session_expiry_helpers() {
        let start = Instant::now();
        assert!(!is_session_ws_expired(
            start,
            start + Duration::from_secs(60)
        ));
        assert!(is_session_ws_expired(
            start,
            start + Duration::from_millis(SESSION_WEBSOCKET_MAX_AGE_MS + 1)
        ));
        assert!(is_session_ws_idle_expired(
            start,
            start + Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS + 1)
        ));
    }

    #[test]
    fn response_create_envelope_shape() {
        let body = json!({"model":"gpt","input":[{"role":"user","content":"hi"}],"store":false});
        let env = wrap_response_create_envelope(&body);
        assert_eq!(env["type"], "response.create");
        assert_eq!(env["model"], "gpt");
        assert_eq!(env["input"][0]["role"], "user");
        assert_eq!(env["store"], false);
    }

    #[test]
    fn response_create_envelope_type_wins() {
        let body = json!({"type":"other","model":"x"});
        let env = wrap_response_create_envelope(&body);
        assert_eq!(env["type"], "response.create");
        assert_eq!(env["model"], "x");
    }

    #[test]
    fn cached_request_body_sets_previous_response_id() {
        let first = json!({
            "model": "gpt",
            "input": [{"role":"user","content":"a"}],
            "store": false
        });
        let cont = WsContinuation {
            last_request_body: first.clone(),
            last_response_id: "resp-1".into(),
            last_response_items: vec![],
        };
        let second = json!({
            "model": "gpt",
            "input": [
                {"role":"user","content":"a"},
                {"role":"user","content":"b"}
            ],
            "store": false
        });
        let delta = build_cached_websocket_request_body(&second, &cont);
        assert_eq!(delta["previous_response_id"], "resp-1");
        assert_eq!(delta["input"].as_array().unwrap().len(), 1);
        assert_eq!(delta["input"][0]["content"], "b");
    }

    // ----- async mocked-WS integration -----

    #[tokio::test]
    async fn ws_sends_response_create_envelope() {
        let session = format!("sess-envelope-{}", common::now_ms());
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));

        let shared = MockWsShared::default();
        {
            let mut inbound = shared.inbound.lock().await;
            inbound.push(vec![WsMessage::Text(completed_frame("resp-env", "hi"))]);
        }
        let connector = Arc::new(MockWsConnector {
            shared: shared.clone(),
        });

        let options = StreamOptions {
            transport: Some(Transport::Websocket),
            session_id: Some(session.clone()),
            api_key: Some("tok".into()),
            ..Default::default()
        };
        let mut stream = stream_with_ws_connector(
            test_model(),
            test_context("hello"),
            options,
            connector,
            None,
        );
        while stream.next().await.is_some() {}

        let sent = shared.sent.lock().clone();
        assert_eq!(sent.len(), 1, "exactly one WS frame sent");
        let parsed: Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(
            parsed["type"].as_str(),
            Some("response.create"),
            "envelope must be response.create, got {parsed}"
        );
        assert!(parsed.get("model").is_some() || parsed.get("input").is_some());
        close_session_websockets(Some(&session));
        clear_websocket_sse_fallback(Some(&session));
    }

    #[tokio::test]
    async fn connection_limit_retries_once_then_succeeds() {
        let session = format!("sess-retry-{}", common::now_ms());
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));

        let shared = MockWsShared {
            fail_connects: Arc::new(AtomicUsize::new(1)),
            ..Default::default()
        };
        {
            let mut inbound = shared.inbound.lock().await;
            // First successful connect after the failed one.
            inbound.push(vec![WsMessage::Text(completed_frame("resp-retry", "ok"))]);
        }
        let connector = Arc::new(MockWsConnector {
            shared: shared.clone(),
        });
        let http_hits = Arc::new(AtomicUsize::new(0));
        let sse = Arc::new(RecordingHttp {
            hits: http_hits.clone(),
            body: b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"sse\",\"status\":\"completed\",\"output\":[],\"usage\":{}}}\n\n".to_vec(),
        });

        let options = StreamOptions {
            transport: Some(Transport::Websocket),
            session_id: Some(session.clone()),
            api_key: Some("tok".into()),
            ..Default::default()
        };
        let mut stream = stream_with_ws_connector(
            test_model(),
            test_context("retry"),
            options,
            connector,
            Some(sse),
        );
        while stream.next().await.is_some() {}

        assert_eq!(
            shared.connect_count.load(Ordering::SeqCst),
            2,
            "connect once (limit) + one retry"
        );
        assert_eq!(
            http_hits.load(Ordering::SeqCst),
            0,
            "must not fall back to SSE when retry succeeds"
        );
        assert!(
            !is_websocket_sse_fallback_active(Some(&session)),
            "successful retry must not record SSE fallback"
        );
        close_session_websockets(Some(&session));
        clear_websocket_sse_fallback(Some(&session));
    }

    #[tokio::test]
    async fn connection_limit_twice_records_sse_fallback() {
        let session = format!("sess-fallback-{}", common::now_ms());
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));

        let shared = MockWsShared {
            fail_connects: Arc::new(AtomicUsize::new(2)),
            ..Default::default()
        };
        let connector = Arc::new(MockWsConnector {
            shared: shared.clone(),
        });
        let http_hits = Arc::new(AtomicUsize::new(0));
        let sse_body = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"sse-1\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"from-sse\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let sse = Arc::new(RecordingHttp {
            hits: http_hits.clone(),
            body: sse_body.to_vec(),
        });

        let options = StreamOptions {
            transport: Some(Transport::Auto),
            session_id: Some(session.clone()),
            api_key: Some("tok".into()),
            ..Default::default()
        };
        let mut stream = stream_with_ws_connector(
            test_model(),
            test_context("fallback"),
            options,
            connector,
            Some(sse),
        );
        while stream.next().await.is_some() {}

        assert_eq!(
            shared.connect_count.load(Ordering::SeqCst),
            2,
            "attempt + single retry, then stop"
        );
        assert_eq!(http_hits.load(Ordering::SeqCst), 1, "SSE fallback used");
        assert!(
            is_websocket_sse_fallback_active(Some(&session)),
            "session must be recorded for SSE fallback"
        );
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));
    }

    #[tokio::test]
    async fn session_websocket_reused_across_two_requests() {
        let session = format!("sess-reuse-{}", common::now_ms());
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));

        let shared = MockWsShared::default();
        {
            let mut inbound = shared.inbound.lock().await;
            // One connection serves two turns.
            inbound.push(vec![
                WsMessage::Text(completed_frame("resp-1", "one")),
                WsMessage::Text(completed_frame("resp-2", "two")),
            ]);
        }
        let connector = Arc::new(MockWsConnector {
            shared: shared.clone(),
        });

        let options = StreamOptions {
            transport: Some(Transport::WebsocketCached),
            session_id: Some(session.clone()),
            api_key: Some("tok".into()),
            ..Default::default()
        };

        let mut s1 = stream_with_ws_connector(
            test_model(),
            test_context("first"),
            options.clone(),
            connector.clone(),
            None,
        );
        while s1.next().await.is_some() {}

        // Second request extends first input with assistant items + new user turn.
        // baseline = lastInput + lastResponseItems; delta is the single new user item.
        let mut s2 = stream_with_ws_connector(
            test_model(),
            continued_context("first", "one", "resp-1", "second"),
            options,
            connector,
            None,
        );
        while s2.next().await.is_some() {}

        assert_eq!(
            shared.connect_count.load(Ordering::SeqCst),
            1,
            "second request must reuse the session socket"
        );
        let sent = shared.sent.lock().clone();
        assert_eq!(sent.len(), 2, "two response.create frames on one socket");
        let first: Value = serde_json::from_str(&sent[0]).unwrap();
        let second: Value = serde_json::from_str(&sent[1]).unwrap();
        assert_eq!(first["type"], "response.create");
        assert_eq!(second["type"], "response.create");
        assert_eq!(
            second["previous_response_id"].as_str(),
            Some("resp-1"),
            "reused socket must send previous_response_id delta, got {second}"
        );
        let delta = second["input"].as_array().expect("delta input array");
        assert_eq!(
            delta.len(),
            1,
            "delta must be exactly one new input item, got {delta:?}"
        );
        // New user turn is the sole delta item.
        assert_eq!(delta[0]["role"], "user");
        close_session_websockets(Some(&session));
        clear_websocket_sse_fallback(Some(&session));
    }

    #[tokio::test]
    async fn orphaned_turn_channel_close_is_error() {
        // Empty inbound → socket returns None mid-turn → Error (not silent Ok).
        let session = format!("sess-orphan-{}", common::now_ms());
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));

        let shared = MockWsShared::default();
        {
            let mut inbound = shared.inbound.lock().await;
            // Explicit empty queue for the connection: next() → None immediately.
            inbound.push(vec![]);
        }
        let connector = Arc::new(MockWsConnector {
            shared: shared.clone(),
        });
        // Force websocket-only so we don't fall back to SSE and mask the error.
        let options = StreamOptions {
            transport: Some(Transport::Websocket),
            session_id: Some(session.clone()),
            api_key: Some("tok".into()),
            ..Default::default()
        };
        let mut stream = stream_with_ws_connector(
            test_model(),
            test_context("orphan"),
            options,
            connector,
            None,
        );
        let mut saw_error = false;
        while let Some(event) = stream.next().await {
            if matches!(event, AssistantMessageEvent::Error { .. }) {
                saw_error = true;
            }
        }
        assert!(
            saw_error,
            "orphaned/closed turn must surface Error, not silent Ok"
        );
        close_session_websockets(Some(&session));
        clear_websocket_sse_fallback(Some(&session));
    }

    #[tokio::test(start_paused = true)]
    async fn session_ws_proactive_idle_eviction() {
        let session = format!("sess-idle-{}", common::now_ms());
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));

        let shared = MockWsShared::default();
        {
            let mut inbound = shared.inbound.lock().await;
            // First connection: one successful turn, then idle-evicted.
            inbound.push(vec![WsMessage::Text(completed_frame("resp-idle-1", "a"))]);
            // Second connection after eviction.
            inbound.push(vec![WsMessage::Text(completed_frame("resp-idle-2", "b"))]);
        }
        let connector = Arc::new(MockWsConnector {
            shared: shared.clone(),
        });
        let options = StreamOptions {
            transport: Some(Transport::WebsocketCached),
            session_id: Some(session.clone()),
            api_key: Some("tok".into()),
            ..Default::default()
        };

        let mut s1 = stream_with_ws_connector(
            test_model(),
            test_context("idle-1"),
            options.clone(),
            connector.clone(),
            None,
        );
        while s1.next().await.is_some() {}
        assert_eq!(shared.connect_count.load(Ordering::SeqCst), 1);

        // Advance past idle TTL; proactive timer must close the parked session task.
        tokio::time::advance(Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS + 50)).await;
        // Yield so the session task's sleep wakes and removes itself.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let mut s2 = stream_with_ws_connector(
            test_model(),
            test_context("idle-2"),
            options,
            connector,
            None,
        );
        while s2.next().await.is_some() {}

        assert_eq!(
            shared.connect_count.load(Ordering::SeqCst),
            2,
            "idle eviction must force a new websocket connection"
        );
        close_session_websockets(Some(&session));
        clear_websocket_sse_fallback(Some(&session));
    }

    #[tokio::test]
    async fn close_limit_before_start_retries_once() {
        let session = format!("sess-close-limit-{}", common::now_ms());
        clear_websocket_sse_fallback(Some(&session));
        close_session_websockets(Some(&session));

        let shared = MockWsShared {
            // First connection closes with limit on first read; second succeeds.
            close_limit_on_conn: Arc::new(AtomicUsize::new(1)),
            ..Default::default()
        };
        {
            let mut inbound = shared.inbound.lock().await;
            // Conn 1: close happens before inbound is read (close_limit_on_conn).
            inbound.push(vec![]);
            // Conn 2: success.
            inbound.push(vec![WsMessage::Text(completed_frame("resp-ok", "ok"))]);
        }
        let connector = Arc::new(MockWsConnector {
            shared: shared.clone(),
        });
        let http_hits = Arc::new(AtomicUsize::new(0));
        let sse = Arc::new(RecordingHttp {
            hits: http_hits.clone(),
            body: b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"x\",\"status\":\"completed\",\"output\":[],\"usage\":{}}}\n\n".to_vec(),
        });

        let options = StreamOptions {
            transport: Some(Transport::Websocket),
            session_id: Some(session.clone()),
            api_key: Some("tok".into()),
            ..Default::default()
        };
        let mut stream = stream_with_ws_connector(
            test_model(),
            test_context("close-limit"),
            options,
            connector,
            Some(sse),
        );
        while stream.next().await.is_some() {}

        assert_eq!(shared.connect_count.load(Ordering::SeqCst), 2);
        assert_eq!(http_hits.load(Ordering::SeqCst), 0);
        close_session_websockets(Some(&session));
        clear_websocket_sse_fallback(Some(&session));
    }
}
