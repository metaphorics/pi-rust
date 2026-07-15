use std::sync::Arc;

use serde_json::{Value, json};

use crate::{
    event_stream::AssistantMessageEventStream,
    http::{ReqwestStreamHttpClient, StreamHttpClient},
    types::{
        AssistantMessageEvent, Content, Context, Message, Model, StopReason, StreamOptions,
        UserContent,
    },
};

use super::common::{self, ApiResult, EventBuilder};

fn bedrock_messages(context: &Context) -> Vec<Value> {
    context.messages.iter().map(|message| match message {
        Message::User(user) => {
            let blocks = match &user.content { UserContent::Text(text) => vec![json!({"text":text})], UserContent::Blocks(content) => content.iter().filter_map(|item| match item { Content::Text(text) => Some(json!({"text":text.text})), Content::Image(image) => Some(json!({"image":{"format":image.mime_type.rsplit('/').next().unwrap_or("png"),"source":{"bytes":image.data}}})), _ => None }).collect() };
            json!({"role":"user","content":blocks})
        }
        Message::Assistant(assistant) => json!({"role":"assistant","content":assistant.content.iter().filter_map(|item| match item { Content::Text(text) => Some(json!({"text":text.text})), Content::Thinking(thinking) => Some(json!({"reasoningContent":{"reasoningText":{"text":thinking.thinking,"signature":thinking.thinking_signature}}})), Content::ToolCall(call) => Some(json!({"toolUse":{"toolUseId":call.id,"name":call.name,"input":call.arguments}})), Content::Image(_) => None }).collect::<Vec<_>>() }),
        Message::ToolResult(result) => json!({"role":"user","content":[{"toolResult":{"toolUseId":result.tool_call_id,"status":if result.is_error {"error"} else {"success"},"content":[{"text":result.content.iter().filter_map(|item| if let Content::Text(text)=item {Some(text.text.as_str())} else {None}).collect::<Vec<_>>().join("\n")} ]}}]}),
    }).collect()
}

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({"modelId":model.id,"messages":bedrock_messages(context),"inferenceConfig":{"maxTokens":options.max_tokens.unwrap_or(model.max_tokens)}});
    if let Some(system) = &context.system_prompt {
        body["system"] = json!([{"text":system}]);
    }
    if let Some(temperature) = options.temperature {
        body["inferenceConfig"]["temperature"] = json!(temperature);
    }
    if !context.tools.is_empty() {
        body["toolConfig"] = json!({"tools":context.tools.iter().map(|tool| json!({"toolSpec":{"name":tool.name,"description":tool.description,"inputSchema":{"json":tool.parameters}}})).collect::<Vec<_>>()});
    }
    body
}

pub fn build_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = common::merged_headers(model, options);
    headers.push(("content-type".into(), "application/json".into()));
    if let Some(token) = options
        .env
        .as_ref()
        .and_then(|env| env.get("AWS_BEARER_TOKEN_BEDROCK"))
        .or(options.api_key.as_ref())
    {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    headers
}

pub fn parse_stream_events<I, B>(chunks: I, model: &Model) -> ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(chunk.as_ref());
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| format!("invalid Bedrock JSON stream UTF-8: {error}"))?;
    let mut builder = EventBuilder::new(model);
    let mut reason = StopReason::Stop;
    let mut block_keys = std::collections::HashMap::<u64, String>::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid Bedrock stream JSON: {error}"))?;
        if let Some(start) = event.get("contentBlockStart") {
            let index = start["contentBlockIndex"].as_u64().unwrap_or(0);
            let tool = &start["start"]["toolUse"];
            if tool.is_object() {
                let key = index.to_string();
                block_keys.insert(index, key.clone());
                builder.tool_call_start(
                    &key,
                    tool["toolUseId"].as_str().unwrap_or(&key),
                    tool["name"].as_str().unwrap_or(""),
                );
            }
        }
        if let Some(delta) = event.get("contentBlockDelta") {
            let index = delta["contentBlockIndex"].as_u64().unwrap_or(0);
            let value = &delta["delta"];
            if let Some(text) = value["text"].as_str() {
                builder.text_delta(text);
            }
            if let Some(text) = value
                .pointer("/reasoningContent/text")
                .and_then(Value::as_str)
            {
                builder.thinking_delta(text);
            }
            if let Some(signature) = value
                .pointer("/reasoningContent/signature")
                .and_then(Value::as_str)
            {
                builder.set_thinking_signature(signature.to_owned());
            }
            if let Some(args) = value.pointer("/toolUse/input").and_then(Value::as_str) {
                let key = block_keys
                    .get(&index)
                    .cloned()
                    .unwrap_or_else(|| index.to_string());
                builder.tool_call_delta(&key, args);
            }
        }
        if let Some(stop) = event.get("messageStop") {
            reason = common::stop_reason(stop["stopReason"].as_str());
        }
        if let Some(metadata) = event.get("metadata") {
            let usage = &metadata["usage"];
            builder.set_usage(
                usage["inputTokens"].as_u64(),
                usage["outputTokens"].as_u64(),
                usage["cacheReadInputTokens"].as_u64(),
                usage["cacheWriteInputTokens"].as_u64(),
                None,
            );
        }
        if let Some(message) = event
            .pointer("/modelStreamErrorException/message")
            .and_then(Value::as_str)
        {
            return Err(message.to_owned());
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
    let url = format!(
        "{}/model/{}/converse-stream",
        model.base_url.trim_end_matches('/'),
        model.id
    );
    let headers = build_headers(&model, &options);
    let body = build_request_body(&model, &context, &options);
    common::spawn_stream(
        model,
        client,
        common::WireRequest {
            url,
            headers,
            body,
            json_stream: true,
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
