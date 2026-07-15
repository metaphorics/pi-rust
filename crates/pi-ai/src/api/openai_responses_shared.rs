use serde_json::Value;

use crate::{sse::parse_sse_chunks, types::{AssistantMessageEvent, Model, StopReason}};

use super::common::{self, ApiResult, EventBuilder};

pub fn parse_responses_stream<I, B>(chunks: I, model: &Model) -> ApiResult<Vec<AssistantMessageEvent>>
where I: IntoIterator<Item=B>, B: AsRef<[u8]> {
    let mut builder = EventBuilder::new(model);
    let mut reason = StopReason::Stop;
    let mut calls = std::collections::HashMap::<String, String>::new();
    for data in parse_sse_chunks(chunks) {
        if data == "[DONE]" { break; }
        let event: Value = serde_json::from_str(&data).map_err(|error| format!("invalid Responses SSE JSON: {error}"))?;
        match event["type"].as_str() {
            Some("response.created" | "response.in_progress") => {
                let response = &event["response"];
                builder.set_response_id(response["id"].as_str());
                builder.set_response_model(response["model"].as_str());
            }
            Some("response.output_text.delta") => builder.text_delta(event["delta"].as_str().unwrap_or("")),
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => builder.thinking_delta(event["delta"].as_str().unwrap_or("")),
            Some("response.output_item.added") => {
                let item = &event["item"];
                if item["type"] == "function_call" {
                    let key = item["id"].as_str().or_else(|| item["call_id"].as_str()).unwrap_or("0");
                    calls.insert(item["call_id"].as_str().unwrap_or(key).to_owned(), key.to_owned());
                    builder.tool_call_start(key, item["call_id"].as_str().unwrap_or(key), item["name"].as_str().unwrap_or(""));
                    if let Some(args) = item["arguments"].as_str().filter(|v| !v.is_empty()) { builder.tool_call_delta(key, args); }
                }
            }
            Some("response.function_call_arguments.delta") => {
                let raw = event["item_id"].as_str().or_else(|| event["call_id"].as_str()).unwrap_or("0");
                let key = calls.get(raw).map_or(raw, String::as_str);
                builder.tool_call_delta(key, event["delta"].as_str().unwrap_or(""));
            }
            Some("response.reasoning_summary_part.added") => {
                if let Some(signature) = event.pointer("/part/encrypted_content").and_then(Value::as_str) { builder.set_thinking_signature(signature.to_owned()); }
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                let response = &event["response"];
                builder.set_response_id(response["id"].as_str());
                let usage = &response["usage"];
                builder.set_usage(usage["input_tokens"].as_u64(), usage["output_tokens"].as_u64(), usage.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64), None, usage.pointer("/output_tokens_details/reasoning_tokens").and_then(Value::as_u64));
                reason = common::stop_reason(response["status"].as_str());
            }
            Some("error") => return Err(event["message"].as_str().unwrap_or("Responses stream error").to_owned()),
            _ => {}
        }
    }
    Ok(builder.finish(reason))
}
