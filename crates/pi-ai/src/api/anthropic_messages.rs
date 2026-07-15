use std::sync::Arc;

use serde_json::{Value, json};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions},
};

use super::{
    common::{self, ApiResult},
    transform_messages,
};

pub const CLAUDE_CODE_SYSTEM_PROMPT: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
pub const CLAUDE_CODE_TOOL_PREFIX: &str = "mcp_";

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let tools: Vec<Value> = context
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect();
    let mut body = json!({
        "model": model.id,
        "messages": transform_messages::anthropic_messages(context),
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "stream": true,
    });
    if options
        .api_key
        .as_deref()
        .is_some_and(|key| key.starts_with("sk-ant-oat"))
    {
        let mut system = vec![json!({"type":"text","text":CLAUDE_CODE_SYSTEM_PROMPT})];
        if let Some(prompt) = &context.system_prompt {
            system.push(json!({"type":"text","text":prompt}));
        }
        body["system"] = Value::Array(system);
    } else if let Some(system) = &context.system_prompt {
        body["system"] = Value::String(system.clone());
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("anthropic-version".into(), "2023-06-01".into()));
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(key) = &options.api_key {
        if key.starts_with("sk-ant-oat") {
            headers.push(("authorization".into(), format!("Bearer {key}")));
            headers.push((
                "anthropic-beta".into(),
                "claude-code-20250219,oauth-2025-04-20".into(),
            ));
            headers.push(("accept".into(), "application/json".into()));
            headers.push((
                "anthropic-dangerous-direct-browser-access".into(),
                "true".into(),
            ));
            headers.push(("user-agent".into(), "claude-cli/2.1.75".into()));
            headers.push(("x-app".into(), "cli".into()));
        } else {
            headers.push(("x-api-key".into(), key.clone()));
        }
    }
    headers
}

pub fn parse_stream_events<I, B>(chunks: I, model: &Model) -> ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    common::decode_sse_chunks(chunks, super::incremental::decoder(model))
}

pub fn stream_with_client(
    model: Model,
    context: Context,
    options: StreamOptions,
    client: Arc<dyn StreamHttpClient>,
) -> AssistantMessageEventStream {
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
    let headers = build_headers(&model, &options);
    let body = build_request_body(&model, &context, &options);
    common::spawn_stream(
        model,
        client,
        common::WireRequest {
            url,
            headers,
            body,
            json_stream: false,
        },
    )
}

pub fn stream(
    model: Model,
    context: Context,
    options: StreamOptions,
) -> AssistantMessageEventStream {
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

pub fn stream_simple(
    model: Model,
    context: Context,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    stream(model, context, options)
}
