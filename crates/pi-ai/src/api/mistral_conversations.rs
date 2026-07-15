use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use serde_json::{Value, json};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    sse::parse_sse_chunks,
    types::{AssistantMessageEvent, Content, Context, Message, Model, StopReason, StreamOptions},
};

use super::{
    common::{self, EventBuilder},
    transform_messages,
};

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut output = Vec::new();
    while value > 0 {
        let digit = (value % 36) as u8;
        output.push(if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        });
        value /= 36;
    }
    output.reverse();
    String::from_utf8(output).unwrap_or_default()
}

fn short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beefu32;
    let mut h2 = 0x41c6_ce57u32;
    for unit in value.encode_utf16() {
        let unit = u32::from(unit);
        h1 = (h1 ^ unit).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ unit).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn normalize_mistral_ids(messages: &mut [Value]) {
    let mut ids = HashMap::<String, String>::new();
    for message in messages {
        if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
            for call in calls {
                if let Some(id) = call["id"].as_str() {
                    let normalized = id
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .collect::<String>();
                    let normalized = if normalized.len() == 9 {
                        normalized
                    } else {
                        short_hash(if normalized.is_empty() {
                            id
                        } else {
                            &normalized
                        })
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .take(9)
                        .collect()
                    };
                    ids.insert(id.to_owned(), normalized.clone());
                    call["id"] = Value::String(normalized);
                }
            }
        }
        if let Some(id) = message["tool_call_id"].as_str()
            && let Some(normalized) = ids.get(id)
        {
            message["tool_call_id"] = Value::String(normalized.clone());
        }
    }
}

fn mistral_messages(context: &Context) -> Vec<Value> {
    let mut reasoning = context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(
                assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::Thinking(thinking) => Some(thinking.thinking.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<VecDeque<_>>();
    let mut messages = transform_messages::openai_messages(context);
    for message in &mut messages {
        if message["role"] != "assistant" {
            continue;
        }
        let thinking = reasoning.pop_front().unwrap_or_default();
        if thinking.is_empty() {
            continue;
        }
        let text = message["content"].as_str().unwrap_or("").to_owned();
        let mut content = vec![json!({
            "type":"thinking",
            "thinking":[{"type":"text","text":thinking}]
        })];
        if !text.is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
        message["content"] = Value::Array(content);
    }
    normalize_mistral_ids(&mut messages);
    messages
}

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let messages = mistral_messages(context);
    let mut body = json!({"model":model.id,"stream":true,"messages":messages,"max_tokens":options.max_tokens.unwrap_or(model.max_tokens)});
    let tools = transform_messages::openai_tools(context);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }
    if model.reasoning {
        body["prompt_mode"] = Value::String("reasoning".into());
    }
    body
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(key) = &options.api_key {
        headers.push(("authorization".into(), format!("Bearer {key}")));
    }
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
    let mut builder = EventBuilder::new(model);
    let mut reason = StopReason::Stop;
    for data in parse_sse_chunks(chunks) {
        if data == "[DONE]" {
            break;
        }
        let event: Value = serde_json::from_str(&data)
            .map_err(|error| format!("invalid Mistral SSE JSON: {error}"))?;
        let chunk = event
            .get("data")
            .filter(|value| value.is_object())
            .unwrap_or(&event);
        builder.set_response_id(chunk["id"].as_str());
        builder.set_response_model(chunk["model"].as_str());
        if let Some(usage) = chunk.get("usage") {
            builder.set_usage(
                usage["prompt_tokens"]
                    .as_u64()
                    .or_else(|| usage["promptTokens"].as_u64()),
                usage["completion_tokens"]
                    .as_u64()
                    .or_else(|| usage["completionTokens"].as_u64()),
                usage["cached_tokens"]
                    .as_u64()
                    .or_else(|| usage["cachedTokens"].as_u64()),
                None,
                None,
            );
        }
        let Some(choice) = chunk["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            continue;
        };
        let delta = &choice["delta"];
        match &delta["content"] {
            Value::String(text) => builder.text_delta(text),
            Value::Array(parts) => {
                for part in parts {
                    match part["type"].as_str() {
                        Some("text") => builder.text_delta(part["text"].as_str().unwrap_or("")),
                        Some("thinking") => {
                            if let Some(items) = part["thinking"].as_array() {
                                for item in items {
                                    builder.thinking_delta(item["text"].as_str().unwrap_or(""));
                                }
                            } else {
                                builder.thinking_delta(
                                    part["thinking"]
                                        .as_str()
                                        .or_else(|| part["text"].as_str())
                                        .unwrap_or(""),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let key = call["index"].as_u64().map_or_else(
                    || call["id"].as_str().unwrap_or("0").to_owned(),
                    |index| index.to_string(),
                );
                if call["id"].is_string() || call.pointer("/function/name").is_some() {
                    builder.tool_call_start(
                        &key,
                        call["id"].as_str().unwrap_or(&key),
                        call.pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    builder.tool_call_delta(&key, arguments);
                }
            }
        }
        if choice["finish_reason"].is_string() {
            reason = common::stop_reason(choice["finish_reason"].as_str());
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
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
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
