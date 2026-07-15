use std::{collections::HashMap, sync::Arc};

use serde_json::{Value, json};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    json_parse::parse_streaming_json,
    sse::parse_sse_chunks,
    types::{
        AssistantMessage, AssistantMessageEvent, Content, Context, Model, StopReason,
        StreamOptions, TextContent, ThinkingContent, ToolCall, Usage,
    },
};

use super::common::{self, ApiResult};

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    json!({"model":model.id,"context":context,"options":{
        "temperature":options.temperature,
        "maxTokens":options.max_tokens,
        "cacheRetention":options.cache_retention,
        "sessionId":options.session_id,
    }})
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("accept".into(), "text/event-stream".into()));
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(key) = &options.api_key {
        headers.push(("authorization".into(), format!("Bearer {key}")));
    }
    headers
}

fn set_content(message: &mut AssistantMessage, index: usize, content: Content) {
    while message.content.len() < index {
        message.content.push(Content::Text(TextContent {
            text: String::new(),
            text_signature: None,
        }));
    }
    if index == message.content.len() {
        message.content.push(content);
    } else {
        message.content[index] = content;
    }
}

fn event_index(event: &Value) -> usize {
    event["contentIndex"].as_u64().unwrap_or(0) as usize
}

fn apply_usage(message: &mut AssistantMessage, usage: &Value) {
    if let Ok(parsed) = serde_json::from_value::<Usage>(usage.clone()) {
        message.usage = parsed;
        return;
    }
    message.usage.input = usage["input"].as_u64().unwrap_or(0);
    message.usage.output = usage["output"].as_u64().unwrap_or(0);
    message.usage.cache_read = usage["cacheRead"].as_u64().unwrap_or(0);
    message.usage.cache_write = usage["cacheWrite"].as_u64().unwrap_or(0);
    message.usage.reasoning = usage["reasoning"].as_u64();
    message.usage.total_tokens = usage["totalTokens"].as_u64().unwrap_or_else(|| {
        message.usage.input
            + message.usage.output
            + message.usage.cache_read
            + message.usage.cache_write
    });
}

pub fn parse_stream_events<I, B>(chunks: I, model: &Model) -> ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut message = common::empty_message(model);
    let mut events = Vec::new();
    let mut tool_json = HashMap::<usize, String>::new();
    for data in parse_sse_chunks(chunks) {
        let event: Value = serde_json::from_str(&data)
            .map_err(|error| format!("invalid pi-messages SSE JSON: {error}"))?;
        match event["type"].as_str() {
            Some("start") => events.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            }),
            Some("text_start") => {
                let index = event_index(&event);
                set_content(
                    &mut message,
                    index,
                    Content::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                );
                events.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: message.clone(),
                });
            }
            Some("text_delta") => {
                let index = event_index(&event);
                let delta = event["delta"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Text(text)) = message.content.get_mut(index) {
                    text.text.push_str(&delta);
                }
                events.push(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial: message.clone(),
                });
            }
            Some("text_end") => {
                let index = event_index(&event);
                let content = event["content"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Text(text)) = message.content.get_mut(index) {
                    text.text.clone_from(&content);
                    text.text_signature = event["contentSignature"].as_str().map(str::to_owned);
                }
                events.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: message.clone(),
                });
            }
            Some("thinking_start") => {
                let index = event_index(&event);
                set_content(
                    &mut message,
                    index,
                    Content::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                );
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: message.clone(),
                });
            }
            Some("thinking_delta") => {
                let index = event_index(&event);
                let delta = event["delta"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Thinking(thinking)) = message.content.get_mut(index) {
                    thinking.thinking.push_str(&delta);
                }
                events.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial: message.clone(),
                });
            }
            Some("thinking_end") => {
                let index = event_index(&event);
                let content = event["content"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Thinking(thinking)) = message.content.get_mut(index) {
                    thinking.thinking.clone_from(&content);
                    thinking.thinking_signature =
                        event["contentSignature"].as_str().map(str::to_owned);
                    thinking.redacted = event["redacted"].as_bool();
                }
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: message.clone(),
                });
            }
            Some("toolcall_start") => {
                let index = event_index(&event);
                set_content(
                    &mut message,
                    index,
                    Content::ToolCall(ToolCall {
                        id: event["id"].as_str().unwrap_or("").to_owned(),
                        name: event["toolName"].as_str().unwrap_or("").to_owned(),
                        arguments: HashMap::new(),
                        thought_signature: None,
                    }),
                );
                tool_json.insert(index, String::new());
                events.push(AssistantMessageEvent::ToolcallStart {
                    content_index: index,
                    partial: message.clone(),
                });
            }
            Some("toolcall_delta") => {
                let index = event_index(&event);
                let delta = event["delta"].as_str().unwrap_or("").to_owned();
                let json = tool_json.entry(index).or_default();
                json.push_str(&delta);
                if let Some(Content::ToolCall(call)) = message.content.get_mut(index)
                    && let Some(arguments) = parse_streaming_json(json).as_object()
                {
                    call.arguments = arguments
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                }
                events.push(AssistantMessageEvent::ToolcallDelta {
                    content_index: index,
                    delta,
                    partial: message.clone(),
                });
            }
            Some("toolcall_end") => {
                let index = event_index(&event);
                let tool_call = serde_json::from_value::<ToolCall>(event["toolCall"].clone())
                    .map_err(|error| format!("invalid pi tool call: {error}"))?;
                set_content(&mut message, index, Content::ToolCall(tool_call.clone()));
                tool_json.remove(&index);
                events.push(AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call,
                    partial: message.clone(),
                });
            }
            Some("done") => {
                let reason = serde_json::from_value::<StopReason>(event["reason"].clone())
                    .unwrap_or(StopReason::Stop);
                message.stop_reason = reason;
                message.response_id = event["responseId"].as_str().map(str::to_owned);
                apply_usage(&mut message, &event["usage"]);
                events.push(AssistantMessageEvent::Done {
                    reason,
                    message: message.clone(),
                });
                break;
            }
            Some("error") => {
                let reason = serde_json::from_value::<StopReason>(event["reason"].clone())
                    .unwrap_or(StopReason::Error);
                message.stop_reason = reason;
                message.response_id = event["responseId"].as_str().map(str::to_owned);
                message.error_message = event["errorMessage"].as_str().map(str::to_owned);
                apply_usage(&mut message, &event["usage"]);
                events.push(AssistantMessageEvent::Error {
                    reason,
                    error: message.clone(),
                });
                break;
            }
            _ => {}
        }
    }
    if !events
        .last()
        .is_some_and(AssistantMessageEvent::is_complete)
    {
        return Err("pi-messages stream ended without a terminal event".into());
    }
    Ok(events)
}

pub fn stream_with_client(
    model: Model,
    context: Context,
    options: StreamOptions,
    client: Arc<dyn StreamHttpClient>,
) -> AssistantMessageEventStream {
    let url = format!("{}/messages", model.base_url.trim_end_matches('/'));
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
