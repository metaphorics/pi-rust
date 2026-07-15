use std::sync::Arc;

use serde_json::Value;

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions},
};

use super::{common, openai_responses, openai_responses_shared};

pub const DEFAULT_API_VERSION: &str = "2025-04-01-preview";

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    openai_responses::build_request_body(model, context, options)
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(key) = &options.api_key {
        headers.push(("api-key".into(), key.clone()));
    }
    headers
}

pub fn build_url(model: &Model, options: &StreamOptions) -> String {
    let version = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("apiVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_API_VERSION);
    let base = model.base_url.trim_end_matches('/');
    let path = if base.ends_with("/responses") {
        base.to_owned()
    } else {
        format!("{base}/openai/responses")
    };
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}api-version={version}")
}

pub fn parse_stream_events<I, B>(
    chunks: I,
    model: &Model,
) -> common::ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    openai_responses_shared::parse_responses_stream(chunks, model)
}

pub fn stream_with_client(
    model: Model,
    context: Context,
    options: StreamOptions,
    client: Arc<dyn StreamHttpClient>,
) -> AssistantMessageEventStream {
    let url = build_url(&model, &options);
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
