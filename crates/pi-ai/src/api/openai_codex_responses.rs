//! OpenAI Codex responses transport.
//!
//! Ports oracle `openai-codex-responses.ts`:
//! - WebSocket transport with session-scoped SSE fallback cache (:60-68, :275-348)
//! - zstd request compression on the SSE path (level 3)
//! - SSE parse shared with openai-responses via the incremental decoder
//!
//! Live network is never used in tests: WS is behind [`WebSocketConnector`] and
//! pure helpers (compression, fallback decisions, cache TTL) are unit-tested.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{
        AssistantMessageEvent, Context, Model, StopReason, StreamOptions, Transport,
    },
};

use super::{common, openai_responses};

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
static WS_SESSION_CREATED_AT: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
        .or_else(|| {
            options
                .api_key
                .as_deref()
                .and_then(extract_account_id)
        })
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

pub fn is_session_ws_expired(created_at: Instant, now: Instant) -> bool {
    now.duration_since(created_at) >= Duration::from_millis(SESSION_WEBSOCKET_MAX_AGE_MS)
}

pub fn is_session_ws_idle_expired(last_used: Instant, now: Instant) -> bool {
    now.duration_since(last_used) >= Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS)
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

    let transport = options.transport.unwrap_or(Transport::Auto);
    let session_id = options.session_id.clone();
    let force_sse = matches!(transport, Transport::Sse)
        || is_websocket_sse_fallback_active(session_id.as_deref());

    // Injectable HTTP path always uses SSE (tests inject MockHttp).
    // Live `stream()` attempts WS first when transport is auto/websocket.
    let _ = force_sse;

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
    let transport = options.transport.unwrap_or(Transport::Auto);
    let session_id = options.session_id.clone();
    let ws_disabled = matches!(transport, Transport::Sse)
        || is_websocket_sse_fallback_active(session_id.as_deref());

    if matches!(transport, Transport::Websocket | Transport::WebsocketCached | Transport::Auto)
        && !ws_disabled
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
            let mut websocket_started = false;
            match process_websocket_stream(
                &ws_url,
                &ws_headers,
                &body,
                &model,
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
                    if should_fallback_to_sse(&error, websocket_started) {
                        record_websocket_sse_fallback(session_id.as_deref());
                        // Fall through to SSE via injected-style client.
                    } else {
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
            match ReqwestStreamHttpClient::new() {
                Ok(client) => {
                    let sse_stream =
                        stream_with_client(model, context, options, Arc::new(client));
                    // Bridge: drain sse_stream into producer.
                    let mut sse_stream = sse_stream;
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
            }
        });
        return stream;
    }

    match ReqwestStreamHttpClient::new() {
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
    }
}

async fn process_websocket_stream(
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    model: &Model,
    websocket_started: &mut bool,
    producer: AssistantMessageEventStream,
) -> common::ApiResult<()> {
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

    let (mut socket, _) = connect_async(request)
        .await
        .map_err(|error| format!("websocket connect: {error}"))?;

    if let Some(session) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("session-id"))
        .map(|(_, v)| v.clone())
    {
        let mut guard = WS_SESSION_CREATED_AT.lock();
        let now = Instant::now();
        if let Some(created) = guard.get(&session).copied()
            && is_session_ws_expired(created, now)
        {
            guard.remove(&session);
        }
        guard.entry(session).or_insert(now);
    }

    let payload = serde_json::to_string(body).map_err(|error| error.to_string())?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .map_err(|error| format!("websocket send: {error}"))?;

    let mut decoder = super::incremental::decoder(model);
    while let Some(message) = socket.next().await {
        let message = message.map_err(|error| format!("websocket read: {error}"))?;
        match message {
            Message::Text(text) => {
                *websocket_started = true;
                // Codex WS frames are JSON events (same shape as SSE data payloads).
                for event in decoder.push_frame(&text)? {
                    producer.push(event);
                }
            }
            Message::Binary(bytes) => {
                *websocket_started = true;
                let text = String::from_utf8_lossy(&bytes);
                for event in decoder.push_frame(&text)? {
                    producer.push(event);
                }
            }
            Message::Close(frame) => {
                if let Some(frame) = frame {
                    if frame.code == WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE.into() {
                        return Err(format!(
                            "websocket close {WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE}: {}",
                            frame.reason
                        ));
                    }
                    if frame.reason.contains(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE) {
                        return Err(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE.into());
                    }
                }
                break;
            }
            Message::Ping(data) => {
                let _ = socket.send(Message::Pong(data)).await;
            }
            Message::Pong(_) | Message::Frame(_) => {}
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

#[cfg(test)]
mod tests {
    use super::*;

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
        clear_websocket_sse_fallback(None);
        assert!(!is_websocket_sse_fallback_active(Some("s1")));
        record_websocket_sse_fallback(Some("s1"));
        assert!(is_websocket_sse_fallback_active(Some("s1")));
        assert!(!is_websocket_sse_fallback_active(Some("s2")));
        clear_websocket_sse_fallback(Some("s1"));
        assert!(!is_websocket_sse_fallback_active(Some("s1")));
    }

    #[test]
    fn fallback_decision_before_start() {
        assert!(should_fallback_to_sse("websocket connect failed", false));
        assert!(should_fallback_to_sse(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE, false));
        assert!(!should_fallback_to_sse("protocol error", true));
    }

    #[test]
    fn session_expiry_helpers() {
        let start = Instant::now();
        assert!(!is_session_ws_expired(start, start + Duration::from_secs(60)));
        assert!(is_session_ws_expired(
            start,
            start + Duration::from_millis(SESSION_WEBSOCKET_MAX_AGE_MS + 1)
        ));
        assert!(is_session_ws_idle_expired(
            start,
            start + Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS + 1)
        ));
    }
}
