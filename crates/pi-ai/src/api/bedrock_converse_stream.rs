//! Bedrock Converse Stream native transport.
//!
//! - Bearer-token bypass when `AWS_BEARER_TOKEN_BEDROCK` / api_key is set
//!   (oracle bedrock-converse-stream.ts:93-98).
//! - Otherwise AWS SigV4 via hand-rolled signer + env/profile credential chain.
//! - Response body is `application/vnd.amazon.eventstream` binary frames
//!   decoded by [`super::aws_eventstream`], then mapped by the shared
//!   incremental converse-stream event decoder.

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{
        AssistantMessageEvent, Content, Context, Message, Model, StopReason, StreamOptions,
        UserContent,
    },
};

use super::{
    aws_eventstream::{self, EventStreamDecoder},
    aws_sigv4,
    common::{self, ApiResult},
};

fn bedrock_messages(context: &Context) -> Vec<Value> {
    context
        .messages
        .iter()
        .map(|message| match message {
            Message::User(user) => {
                let blocks = match &user.content {
                    UserContent::Text(text) => vec![json!({"text":text})],
                    UserContent::Blocks(content) => content
                        .iter()
                        .filter_map(|item| match item {
                            Content::Text(text) => Some(json!({"text":text.text})),
                            Content::Image(image) => Some(json!({
                                "image":{
                                    "format":image.mime_type.rsplit('/').next().unwrap_or("png"),
                                    "source":{"bytes":image.data}
                                }
                            })),
                            _ => None,
                        })
                        .collect(),
                };
                json!({"role":"user","content":blocks})
            }
            Message::Assistant(assistant) => json!({
                "role":"assistant",
                "content": assistant.content.iter().filter_map(|item| match item {
                    Content::Text(text) => Some(json!({"text":text.text})),
                    Content::Thinking(thinking) => Some(json!({
                        "reasoningContent":{
                            "reasoningText":{
                                "text":thinking.thinking,
                                "signature":thinking.thinking_signature
                            }
                        }
                    })),
                    Content::ToolCall(call) => Some(json!({
                        "toolUse":{
                            "toolUseId":call.id,
                            "name":call.name,
                            "input":call.arguments
                        }
                    })),
                    Content::Image(_) => None,
                }).collect::<Vec<_>>()
            }),
            Message::ToolResult(result) => json!({
                "role":"user",
                "content":[{
                    "toolResult":{
                        "toolUseId":result.tool_call_id,
                        "status": if result.is_error {"error"} else {"success"},
                        "content":[{
                            "text": result.content.iter().filter_map(|item| {
                                if let Content::Text(text) = item {
                                    Some(text.text.as_string())
                                } else {
                                    None
                                }
                            }).collect::<Vec<_>>().join("\n")
                        }]
                    }
                }]
            }),
        })
        .collect()
}

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "messages": bedrock_messages(context),
        "inferenceConfig": {
            "maxTokens": options.max_tokens.unwrap_or(model.max_tokens)
        }
    });
    if let Some(system) = &context.system_prompt {
        body["system"] = json!([{"text": system}]);
    }
    if let Some(temperature) = options.temperature {
        body["inferenceConfig"]["temperature"] = json!(temperature);
    }
    if !context.tools.is_empty() {
        body["toolConfig"] = json!({
            "tools": context.tools.iter().map(|tool| json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": {"json": tool.parameters}
                }
            })).collect::<Vec<_>>()
        });
    }
    body
}

fn env_value<'a>(options: &'a StreamOptions, key: &str) -> Option<&'a str> {
    options
        .env
        .as_ref()
        .and_then(|env| env.get(key))
        .map(String::as_str)
}

fn bearer_token(options: &StreamOptions) -> Option<String> {
    let skip = env_value(options, "AWS_BEDROCK_SKIP_AUTH") == Some("1");
    if skip {
        return None;
    }
    env_value(options, "AWS_BEARER_TOKEN_BEDROCK")
        .map(str::to_owned)
        .or_else(|| options.api_key.clone())
}

/// Build request headers. Bearer path skips SigV4 (oracle :93-98).
pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    build_headers_for_url(
        &format!(
            "{}/model/{}/converse-stream",
            model.base_url.trim_end_matches('/'),
            model.id
        ),
        &build_request_body(model, &Context::default(), options),
        model,
        options,
    )
}

pub fn build_headers_for_url(
    url: &str,
    body: &Value,
    model: &Model,
    options: &StreamOptions,
) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), "application/vnd.amazon.eventstream".into()));

    if let Some(token) = bearer_token(options) {
        headers.push(("authorization".into(), format!("Bearer {token}")));
        return headers;
    }

    if env_value(options, "AWS_BEDROCK_SKIP_AUTH") == Some("1") {
        // Proxy/test path: unsigned dummy identity (oracle :177-182).
        return headers;
    }

    let options_env = options.env.as_ref();
    if let Some(credentials) = aws_sigv4::resolve_credentials(options_env) {
        let region = aws_sigv4::resolve_region(options_env);
        let body_bytes = serde_json::to_vec(body).unwrap_or_default();
        if let Ok(signed) = aws_sigv4::sign_post_headers(
            url,
            &body_bytes,
            &credentials,
            &region,
            "bedrock",
            &aws_sigv4::amz_date_now(),
        ) {
            // Prefer signed authorization headers over any prior ones.
            headers.retain(|(name, _)| {
                let lower = name.to_ascii_lowercase();
                lower != "authorization"
                    && lower != "x-amz-date"
                    && lower != "x-amz-content-sha256"
                    && lower != "x-amz-security-token"
                    && lower != "content-type"
            });
            headers.extend(signed);
        }
    }
    headers
}

/// Decode either binary eventstream frames or legacy JSONL fixtures.
pub fn parse_stream_events<I, B>(chunks: I, model: &Model) -> ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(chunk.as_ref());
    }
    if looks_like_eventstream(&bytes) {
        decode_eventstream_bytes(&bytes, model)
    } else {
        common::decode_json_chunks([bytes], super::incremental::decoder(model))
    }
}

fn looks_like_eventstream(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    // JSONL starts with `{` or whitespace+`{`; eventstream total_len is a big-endian u32.
    let first = bytes.iter().find(|b| !b.is_ascii_whitespace()).copied();
    match first {
        Some(b'{') | Some(b'[') => false,
        _ => {
            let total = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
            total >= 16 && total <= bytes.len().saturating_mul(4).max(bytes.len() + 16)
        }
    }
}

fn decode_eventstream_bytes(bytes: &[u8], model: &Model) -> ApiResult<Vec<AssistantMessageEvent>> {
    let mut decoder = EventStreamDecoder::default();
    let messages = decoder.push(bytes)?;
    decoder.finish()?;
    let mut event_decoder = super::incremental::decoder(model);
    let mut events = event_decoder.initial_events();
    for message in messages {
        let frame = message.bedrock_event_json()?;
        events.extend(event_decoder.push_frame(&frame)?);
    }
    events.extend(event_decoder.finish()?);
    Ok(events)
}

pub fn stream_with_client(
    model: Model,
    context: Context,
    options: StreamOptions,
    client: Arc<dyn StreamHttpClient>,
) -> AssistantMessageEventStream {
    let url = format!(
        "{}/model/{}/converse-stream",
        model.base_url.trim_end_matches('/'),
        model.id
    );
    let body = build_request_body(&model, &context, &options);
    let headers = build_headers_for_url(&url, &body, &model, &options);
    spawn_eventstream(model, client, url, headers, body)
}

fn spawn_eventstream(
    model: Model,
    client: Arc<dyn StreamHttpClient>,
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
) -> AssistantMessageEventStream {
    let mut decoder = super::incremental::decoder(&model);
    let stream = AssistantMessageEventStream::new();
    let producer = stream.clone();
    tokio::spawn(async move {
        let result: ApiResult<()> = async {
            let mut response = client
                .post_json_stream(&url, &headers, &body)
                .await
                .map_err(|error| error.to_string())?;
            let mut frames = EventStreamDecoder::default();
            let mut json_fallback = common::JsonLineParser::default();
            let mut mode: Option<bool> = None; // Some(true)=eventstream, Some(false)=jsonl
            for event in decoder.initial_events() {
                producer.push(event);
            }
            while let Some(chunk) = response.next().await {
                let chunk = chunk.map_err(|error| error.to_string())?;
                if mode.is_none() {
                    mode = Some(
                        looks_like_eventstream(&chunk) || !chunk.is_empty() && chunk[0] != b'{',
                    );
                    // Prefer eventstream when accept header was set and body is binary.
                    if chunk.starts_with(b"{") || chunk.contains(&b'\n') {
                        // Could still be JSONL; re-check with full heuristics.
                        mode = Some(looks_like_eventstream(&chunk));
                    }
                }
                if mode == Some(true) {
                    for message in frames.push(&chunk)? {
                        let frame = message.bedrock_event_json()?;
                        for event in decoder.push_frame(&frame)? {
                            producer.push(event);
                        }
                    }
                } else {
                    for frame in json_fallback.push(&chunk) {
                        for event in decoder.push_frame(&frame)? {
                            producer.push(event);
                        }
                    }
                }
            }
            if mode == Some(true) {
                for message in frames.finish()? {
                    let frame = message.bedrock_event_json()?;
                    for event in decoder.push_frame(&frame)? {
                        producer.push(event);
                    }
                }
            } else {
                for frame in json_fallback.finish() {
                    for event in decoder.push_frame(&frame)? {
                        producer.push(event);
                    }
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

/// Encode JSONL converse-stream events as real binary eventstream frames (for fixtures/tests).
pub fn encode_jsonl_as_eventstream(jsonl: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(jsonl).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid bedrock jsonl fixture line: {error}"))?;
        let obj = value
            .as_object()
            .ok_or_else(|| "bedrock fixture line must be an object".to_owned())?;
        let (event_type, payload) = obj
            .iter()
            .next()
            .ok_or_else(|| "empty bedrock fixture object".to_owned())?;
        let payload_json = serde_json::to_string(payload)
            .map_err(|error| format!("serialize bedrock payload: {error}"))?;
        out.extend_from_slice(&aws_eventstream::encode_bedrock_event(
            event_type,
            &payload_json,
        )?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, ModelCost, ModelInput};

    fn test_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test".into(),
            api: Api::from("bedrock-converse-stream"),
            provider: "aws".into(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 16_384,
            max_tokens: 128,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn bearer_token_bypasses_sigv4() {
        let mut options = StreamOptions {
            api_key: Some("bedrock-bearer".into()),
            ..Default::default()
        };
        let headers: std::collections::HashMap<_, _> =
            build_headers(&test_model(), &options).into_iter().collect();
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer bedrock-bearer")
        );

        options.api_key = None;
        options.env = Some(std::collections::HashMap::from([(
            "AWS_BEARER_TOKEN_BEDROCK".into(),
            "env-bearer".into(),
        )]));
        let headers: std::collections::HashMap<_, _> =
            build_headers(&test_model(), &options).into_iter().collect();
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer env-bearer")
        );
    }

    #[test]
    fn encodes_and_parses_eventstream_fixture() {
        let jsonl = br#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"Hello"}}}
{"contentBlockStop":{"contentBlockIndex":0}}
{"messageStop":{"stopReason":"end_turn"}}
{"metadata":{"usage":{"inputTokens":1,"outputTokens":1}}}
"#;
        let binary = encode_jsonl_as_eventstream(jsonl).unwrap();
        assert!(!binary.is_empty());
        assert_ne!(binary[0], b'{');
        let events = parse_stream_events([binary.as_slice()], &test_model()).unwrap();
        let types: Vec<_> = events
            .iter()
            .map(|e| match e {
                AssistantMessageEvent::Start { .. } => "start",
                AssistantMessageEvent::TextStart { .. } => "text_start",
                AssistantMessageEvent::TextDelta { .. } => "text_delta",
                AssistantMessageEvent::TextEnd { .. } => "text_end",
                AssistantMessageEvent::Done { .. } => "done",
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            types,
            ["start", "text_start", "text_delta", "text_end", "done"]
        );
    }
}
