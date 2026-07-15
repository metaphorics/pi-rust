use std::sync::Arc;

use serde_json::Value;

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions},
};

use super::{common, google_shared};

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    google_shared::build_request_body(model, context, options)
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(token) = &options.api_key {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    headers
}

pub fn build_url(model: &Model, options: &StreamOptions) -> String {
    let metadata = options.metadata.as_ref();
    let project = metadata
        .and_then(|m| m.get("project"))
        .and_then(Value::as_str)
        .or_else(|| {
            options
                .env
                .as_ref()
                .and_then(|e| e.get("GOOGLE_CLOUD_PROJECT").map(String::as_str))
        })
        .unwrap_or("PROJECT_ID");
    let location = metadata
        .and_then(|m| m.get("location"))
        .and_then(Value::as_str)
        .or_else(|| {
            options
                .env
                .as_ref()
                .and_then(|e| e.get("GOOGLE_CLOUD_LOCATION").map(String::as_str))
        })
        .unwrap_or("global");
    if model.base_url.contains("{project}") || model.base_url.contains("{location}") {
        return model
            .base_url
            .replace("{project}", project)
            .replace("{location}", location)
            .replace("{model}", &model.id);
    }
    let host = if location == "global" {
        "aiplatform.googleapis.com".to_owned()
    } else {
        format!("{location}-aiplatform.googleapis.com")
    };
    format!(
        "https://{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
        model.id
    )
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
