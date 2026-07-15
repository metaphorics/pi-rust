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

fn compat<'a>(model: &'a Model, field: &str) -> Option<&'a Value> {
    model.compat.as_ref()?.get(field)
}

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "model": model.id,
        "messages": transform_messages::openai_messages(context),
        "stream": true,
    });
    if compat(model, "supportsUsageInStreaming").and_then(Value::as_bool) != Some(false) {
        body["stream_options"] = json!({"include_usage":true});
    }
    let max_tokens_field = compat(model, "maxTokensField")
        .and_then(Value::as_str)
        .unwrap_or("max_completion_tokens");
    body[max_tokens_field] = json!(options.max_tokens.unwrap_or(model.max_tokens));
    let tools = transform_messages::openai_tools(context);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }
    if model.reasoning {
        match compat(model, "thinkingFormat").and_then(Value::as_str) {
            Some("openrouter" | "ant-ling") => {
                body["reasoning"] = json!({"effort":"medium"});
            }
            Some("deepseek" | "zai") => {
                body["thinking"] = json!({"type":"enabled"});
                body["reasoning_effort"] = Value::String("medium".into());
            }
            Some("together") => {
                body["reasoning"] = json!({"enabled":true});
                body["reasoning_effort"] = Value::String("medium".into());
            }
            Some("qwen") => body["enable_thinking"] = Value::Bool(true),
            Some("string-thinking") => {
                body["thinking"] = Value::String("medium".into());
            }
            _ => body["reasoning_effort"] = Value::String("medium".into()),
        }
    }
    body
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(key) = &options.api_key {
        headers.push(("authorization".into(), format!("Bearer {key}")));
    }
    if let Some(session) = &options.session_id {
        headers.push(("x-session-id".into(), session.clone()));
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
