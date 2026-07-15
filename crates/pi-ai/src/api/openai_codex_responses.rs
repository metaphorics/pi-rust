use std::sync::Arc;

use serde_json::{Value, json};

use crate::{event_stream::AssistantMessageEventStream, http::{ReqwestStreamHttpClient, StreamHttpClient}, types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions}};

use super::{common, openai_responses, openai_responses_shared};

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_INSTRUCTIONS: &str = "You are a coding agent. Follow the user's instructions and use tools when needed.";

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = openai_responses::build_request_body(model, context, options);
    body["instructions"] = Value::String(context.system_prompt.clone().unwrap_or_else(|| CODEX_INSTRUCTIONS.into()));
    body["include"] = json!(["reasoning.encrypted_content"]);
    body["store"] = Value::Bool(false);
    body
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = openai_responses::build_headers(model, options);
    headers.push(("originator".into(), "pi".into()));
    headers.push(("openai-beta".into(), "responses=experimental".into()));
    if let Some(account) = options.metadata.as_ref().and_then(|m| m.get("accountId")).and_then(Value::as_str) {
        headers.push(("chatgpt-account-id".into(), account.to_owned()));
    }
    headers
}

pub fn parse_stream_events<I, B>(chunks: I, model: &Model) -> common::ApiResult<Vec<AssistantMessageEvent>>
where I: IntoIterator<Item=B>, B: AsRef<[u8]> { openai_responses_shared::parse_responses_stream(chunks, model) }

pub fn stream_with_client(model: Model, context: Context, options: StreamOptions, client: Arc<dyn StreamHttpClient>) -> AssistantMessageEventStream {
    // Codex websocket transport is intentionally not duplicated: SSE is the
    // protocol-compatible fallback used by the official client as well.
    let url = format!("{}/responses", model.base_url.trim_end_matches('/'));
    let headers = build_headers(&model, &options);
    let body = build_request_body(&model, &context, &options);
    common::spawn_stream(model, context, options, client, url, headers, body, |chunks, model| parse_stream_events(chunks, model), false)
}

pub fn stream(model: Model, context: Context, options: StreamOptions) -> AssistantMessageEventStream {
    match ReqwestStreamHttpClient::new() {
        Ok(client) => stream_with_client(model, context, options, Arc::new(client)),
        Err(error) => { let stream = AssistantMessageEventStream::new(); let mut message = common::empty_message(&model); message.stop_reason = StopReason::Error; message.error_message = Some(error.to_string()); stream.push(AssistantMessageEvent::Error { reason: StopReason::Error, error: message }); stream }
    }
}

pub fn stream_simple(model: Model, context: Context, options: StreamOptions) -> AssistantMessageEventStream { stream(model, context, options) }
