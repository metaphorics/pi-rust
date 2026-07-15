use std::sync::Arc;

use serde_json::{Value, json};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    sse::parse_sse_chunks,
    types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions},
};

use super::{
    common::{self, ApiResult, EventBuilder},
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
    if let Some(system) = &context.system_prompt {
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
            headers.push(("anthropic-beta".into(), "oauth-2025-04-20".into()));
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
    let mut builder = EventBuilder::new(model);
    let mut reason = StopReason::Stop;
    let mut blocks: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for data in parse_sse_chunks(chunks) {
        if data == "[DONE]" {
            break;
        }
        let event: Value = serde_json::from_str(&data)
            .map_err(|error| format!("invalid Anthropic SSE JSON: {error}"))?;
        match event["type"].as_str() {
            Some("message_start") => {
                builder.set_response_id(event.pointer("/message/id").and_then(Value::as_str));
                builder.set_response_model(event.pointer("/message/model").and_then(Value::as_str));
                let usage = &event["message"]["usage"];
                builder.set_usage(
                    usage["input_tokens"].as_u64(),
                    usage["output_tokens"].as_u64(),
                    usage["cache_read_input_tokens"].as_u64(),
                    usage["cache_creation_input_tokens"].as_u64(),
                    None,
                );
            }
            Some("content_block_start") => {
                let index = event["index"].as_u64().unwrap_or(0);
                let block = &event["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let key = index.to_string();
                        blocks.insert(index, key.clone());
                        builder.tool_call_start(
                            &key,
                            block["id"].as_str().unwrap_or(""),
                            block["name"].as_str().unwrap_or(""),
                        );
                        if let Some(input) = block["input"].as_object() {
                            let json = serde_json::to_string(input).unwrap_or_default();
                            if json != "{}" {
                                builder.tool_call_delta(&key, &json);
                            }
                        }
                    }
                    Some(kind) => {
                        blocks.insert(index, kind.to_owned());
                    }
                    None => {}
                }
            }
            Some("content_block_delta") => {
                let index = event["index"].as_u64().unwrap_or(0);
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => builder.text_delta(delta["text"].as_str().unwrap_or("")),
                    Some("thinking_delta") => {
                        builder.thinking_delta(delta["thinking"].as_str().unwrap_or(""))
                    }
                    Some("signature_delta") => builder.set_thinking_signature(
                        delta["signature"].as_str().unwrap_or("").to_owned(),
                    ),
                    Some("input_json_delta") => {
                        let key = blocks
                            .get(&index)
                            .cloned()
                            .unwrap_or_else(|| index.to_string());
                        builder.tool_call_delta(&key, delta["partial_json"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                reason = common::stop_reason(
                    event.pointer("/delta/stop_reason").and_then(Value::as_str),
                );
                builder.set_usage(
                    None,
                    event
                        .pointer("/usage/output_tokens")
                        .and_then(Value::as_u64),
                    None,
                    None,
                    None,
                );
            }
            Some("error") => {
                return Err(event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error")
                    .to_owned());
            }
            _ => {}
        }
    }
    Ok(builder.finish(reason))
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
        parse_stream_events,
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
