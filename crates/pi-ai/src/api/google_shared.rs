use serde_json::{Value, json};

use crate::{sse::parse_sse_chunks, types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions}};

use super::{common::{self, ApiResult, EventBuilder}, transform_messages};

pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "contents": transform_messages::google_contents(context),
        "generationConfig": {"maxOutputTokens":options.max_tokens.unwrap_or(model.max_tokens)},
    });
    if let Some(system) = &context.system_prompt { body["systemInstruction"] = json!({"parts":[{"text":system}]}); }
    if let Some(temperature) = options.temperature { body["generationConfig"]["temperature"] = json!(temperature); }
    let tools = transform_messages::google_tools(context);
    if !tools.is_empty() { body["tools"] = Value::Array(tools); }
    if model.reasoning { body["generationConfig"]["thinkingConfig"] = json!({"includeThoughts":true}); }
    body
}

pub fn parse_google_stream<I, B>(chunks: I, model: &Model) -> ApiResult<Vec<AssistantMessageEvent>>
where I: IntoIterator<Item=B>, B: AsRef<[u8]> {
    let mut builder = EventBuilder::new(model);
    let mut reason = StopReason::Stop;
    let data_events = parse_sse_chunks(chunks);
    for data in data_events {
        if data == "[DONE]" { break; }
        let response: Value = serde_json::from_str(&data).map_err(|error| format!("invalid Google stream JSON: {error}"))?;
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) { return Err(error.to_owned()); }
        if let Some(usage) = response.get("usageMetadata") {
            builder.set_usage(usage["promptTokenCount"].as_u64(), usage["candidatesTokenCount"].as_u64(), usage["cachedContentTokenCount"].as_u64(), None, usage["thoughtsTokenCount"].as_u64());
        }
        let Some(candidate) = response["candidates"].as_array().and_then(|v| v.first()) else { continue; };
        if let Some(parts) = candidate.pointer("/content/parts").and_then(Value::as_array) {
            for (index, part) in parts.iter().enumerate() {
                if let Some(text) = part["text"].as_str() {
                    if part["thought"].as_bool() == Some(true) { builder.thinking_delta(text); } else { builder.text_delta(text); }
                    if let Some(signature) = part["thoughtSignature"].as_str() { builder.set_thinking_signature(signature.to_owned()); }
                }
                if let Some(call) = part.get("functionCall") {
                    let key = call["id"].as_str().map(str::to_owned).unwrap_or_else(|| index.to_string());
                    builder.tool_call_start(&key, call["id"].as_str().unwrap_or(&key), call["name"].as_str().unwrap_or(""));
                    let args = serde_json::to_string(&call["args"]).unwrap_or_else(|_| "{}".into());
                    builder.tool_call_delta(&key, &args);
                    if let Some(signature) = part["thoughtSignature"].as_str() { builder.set_tool_signature(&key, signature.to_owned()); }
                }
            }
        }
        if candidate["finishReason"].is_string() { reason = common::stop_reason(candidate["finishReason"].as_str()); }
    }
    Ok(builder.finish(reason))
}
