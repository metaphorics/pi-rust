use std::sync::Arc;

use serde_json::{Value, json};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions},
};

use super::common::{self, ApiResult};

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut wire_options = serde_json::Map::new();
    if let Some(temperature) = options.temperature {
        wire_options.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        wire_options.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(cache_retention) = options.cache_retention {
        wire_options.insert("cacheRetention".into(), json!(cache_retention));
    }
    if let Some(session_id) = &options.session_id {
        wire_options.insert("sessionId".into(), json!(session_id));
    }
    json!({"model":model.id,"context":context,"options":wire_options})
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
