use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::{
    json_parse::parse_streaming_json,
    types::{
        AssistantMessage, AssistantMessageEvent, Content, Model, StopReason, TextContent,
        ThinkingContent, ToolCall, Usage,
    },
};

use super::common::{self, ApiResult, EventBuilder, WireEventDecoder};

pub fn decoder(model: &Model) -> Box<dyn WireEventDecoder> {
    if model.api.as_ref() == "pi-messages" {
        Box::new(PiDecoder::new(model))
    } else {
        Box::new(JsonDecoder::new(model))
    }
}

struct JsonDecoder {
    api: String,
    builder: Option<EventBuilder>,
    reason: StopReason,
    blocks: HashMap<u64, String>,
    calls: HashMap<String, String>,
    complete: bool,
    bedrock_stopped: bool,
}

impl JsonDecoder {
    fn new(model: &Model) -> Self {
        Self {
            api: model.api.as_ref().to_owned(),
            builder: Some(EventBuilder::new(model)),
            reason: StopReason::Stop,
            blocks: HashMap::new(),
            calls: HashMap::new(),
            complete: false,
            bedrock_stopped: false,
        }
    }

    fn builder(&mut self) -> &mut EventBuilder {
        self.builder.as_mut().expect("decoder is not finished")
    }

    fn finalize(&mut self) {
        if self.complete {
            return;
        }
        let reason = self.reason;
        self.builder().finalize(reason);
        self.complete = true;
    }

    fn anthropic(&mut self, event: &Value) -> ApiResult<()> {
        match event["type"].as_str() {
            Some("message_start") => {
                let builder = self.builder();
                builder.set_response_id(event.pointer("/message/id").and_then(Value::as_str));
                builder.set_response_model(event.pointer("/message/model").and_then(Value::as_str));
                let usage = &event["message"]["usage"];
                builder.set_usage(
                    usage["input_tokens"].as_u64(),
                    usage["output_tokens"].as_u64(),
                    usage["cache_read_input_tokens"].as_u64(),
                    usage["cache_creation_input_tokens"].as_u64(),
                    None,
                );
            }
            Some("content_block_start") => {
                let index = event["index"].as_u64().unwrap_or(0);
                let block = &event["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let key = index.to_string();
                        self.blocks.insert(index, format!("tool:{key}"));
                        self.builder().tool_call_start(
                            &key,
                            block["id"].as_str().unwrap_or(""),
                            block["name"].as_str().unwrap_or(""),
                        );
                    }
                    Some(kind) => {
                        self.blocks.insert(index, kind.to_owned());
                    }
                    None => {}
                }
            }
            Some("content_block_delta") => {
                let index = event["index"].as_u64().unwrap_or(0);
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        self.builder()
                            .text_delta(delta["text"].as_str().unwrap_or(""));
                    }
                    Some("thinking_delta") => self
                        .builder()
                        .thinking_delta(delta["thinking"].as_str().unwrap_or("")),
                    Some("signature_delta") => self.builder().set_thinking_signature(
                        delta["signature"].as_str().unwrap_or("").to_owned(),
                    ),
                    Some("input_json_delta") => {
                        let key = self
                            .blocks
                            .get(&index)
                            .and_then(|kind| kind.strip_prefix("tool:"))
                            .map(str::to_owned)
                            .unwrap_or_else(|| index.to_string());
                        self.builder()
                            .tool_call_delta(&key, delta["partial_json"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = event["index"].as_u64().unwrap_or(0);
                match self.blocks.remove(&index).as_deref() {
                    Some("text") => self.builder().end_text(),
                    Some("thinking") => self.builder().end_thinking(),
                    Some(kind) if kind.starts_with("tool:") => {
                        self.builder()
                            .end_tool_call(kind.trim_start_matches("tool:"));
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                self.reason = common::stop_reason(
                    event.pointer("/delta/stop_reason").and_then(Value::as_str),
                );
                self.builder().set_usage(
                    None,
                    event
                        .pointer("/usage/output_tokens")
                        .and_then(Value::as_u64),
                    None,
                    None,
                    None,
                );
            }
            Some("message_stop") => self.finalize(),
            Some("error") => {
                return Err(event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error")
                    .to_owned());
            }
            _ => {}
        }
        Ok(())
    }

    fn completions(&mut self, event: &Value, mistral: bool) {
        let chunk = if mistral {
            event
                .get("data")
                .filter(|value| value.is_object())
                .unwrap_or(event)
        } else {
            event
        };
        self.builder().set_response_id(chunk["id"].as_str());
        self.builder().set_response_model(chunk["model"].as_str());
        if let Some(usage) = chunk.get("usage") {
            self.builder().set_usage(
                usage["prompt_tokens"]
                    .as_u64()
                    .or_else(|| usage["promptTokens"].as_u64()),
                usage["completion_tokens"]
                    .as_u64()
                    .or_else(|| usage["completionTokens"].as_u64()),
                usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64),
                None,
                usage
                    .pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64),
            );
        }
        let Some(choice) = chunk["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            return;
        };
        let delta = &choice["delta"];
        match &delta["content"] {
            Value::String(text) => {
                if mistral {
                    self.builder().end_thinking();
                }
                self.builder().text_delta(text);
            }
            Value::Array(parts) if mistral => {
                for part in parts {
                    match part["type"].as_str() {
                        Some("text") => {
                            self.builder().end_thinking();
                            self.builder()
                                .text_delta(part["text"].as_str().unwrap_or(""));
                        }
                        Some("thinking") => {
                            self.builder().end_text();
                            if let Some(items) = part["thinking"].as_array() {
                                for item in items {
                                    self.builder()
                                        .thinking_delta(item["text"].as_str().unwrap_or(""));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if let Some(thinking) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
        {
            self.builder().thinking_delta(thinking);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            if mistral {
                self.builder().end_text();
                self.builder().end_thinking();
            }
            for call in calls {
                let key = call["index"].as_u64().map_or_else(
                    || call["id"].as_str().unwrap_or("0").to_owned(),
                    |index| index.to_string(),
                );
                if call["id"].is_string() || call.pointer("/function/name").is_some() {
                    self.builder().tool_call_start(
                        &key,
                        call["id"].as_str().unwrap_or(&key),
                        call.pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    self.builder().tool_call_delta(&key, arguments);
                }
            }
        }
        if choice["finish_reason"].is_string() {
            self.reason = common::stop_reason(choice["finish_reason"].as_str());
        }
    }

    fn responses(&mut self, event: &Value) -> ApiResult<()> {
        match event["type"].as_str() {
            Some("response.created" | "response.in_progress") => {
                self.builder()
                    .set_response_id(event.pointer("/response/id").and_then(Value::as_str));
                self.builder()
                    .set_response_model(event.pointer("/response/model").and_then(Value::as_str));
            }
            Some("response.output_text.delta") => {
                self.builder()
                    .text_delta(event["delta"].as_str().unwrap_or(""));
            }
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                self.builder()
                    .thinking_delta(event["delta"].as_str().unwrap_or(""));
            }
            Some("response.output_item.added") => {
                let item = &event["item"];
                if item["type"] == "function_call" {
                    let key = item["id"]
                        .as_str()
                        .or_else(|| item["call_id"].as_str())
                        .unwrap_or("0");
                    self.calls.insert(
                        item["call_id"].as_str().unwrap_or(key).to_owned(),
                        key.to_owned(),
                    );
                    self.builder().tool_call_start(
                        key,
                        item["call_id"].as_str().unwrap_or(key),
                        item["name"].as_str().unwrap_or(""),
                    );
                }
            }
            Some("response.output_item.done") => {
                let item = &event["item"];
                match item["type"].as_str() {
                    Some("message") => self.builder().end_text(),
                    Some("reasoning") => self.builder().end_thinking(),
                    Some("function_call") => {
                        let key = item["id"]
                            .as_str()
                            .or_else(|| item["call_id"].as_str())
                            .unwrap_or("0");
                        self.builder().end_tool_call(key);
                    }
                    _ => {}
                }
            }
            Some("response.function_call_arguments.delta") => {
                let raw = event["item_id"]
                    .as_str()
                    .or_else(|| event["call_id"].as_str())
                    .unwrap_or("0");
                let key = self
                    .calls
                    .get(raw)
                    .cloned()
                    .unwrap_or_else(|| raw.to_owned());
                self.builder()
                    .tool_call_delta(&key, event["delta"].as_str().unwrap_or(""));
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                let response = &event["response"];
                self.builder().set_response_id(response["id"].as_str());
                let usage = &response["usage"];
                self.builder().set_usage(
                    usage["input_tokens"].as_u64(),
                    usage["output_tokens"].as_u64(),
                    usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64),
                    None,
                    usage
                        .pointer("/output_tokens_details/reasoning_tokens")
                        .and_then(Value::as_u64),
                );
                self.reason = common::stop_reason(response["status"].as_str());
                self.finalize();
            }
            Some("error") => {
                return Err(event["message"]
                    .as_str()
                    .unwrap_or("Responses stream error")
                    .to_owned());
            }
            _ => {}
        }
        Ok(())
    }

    fn google(&mut self, response: &Value) -> ApiResult<()> {
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) {
            return Err(error.to_owned());
        }
        if let Some(usage) = response.get("usageMetadata") {
            self.builder().set_usage(
                usage["promptTokenCount"].as_u64(),
                usage["candidatesTokenCount"].as_u64(),
                usage["cachedContentTokenCount"].as_u64(),
                None,
                usage["thoughtsTokenCount"].as_u64(),
            );
        }
        let Some(candidate) = response["candidates"]
            .as_array()
            .and_then(|values| values.first())
        else {
            return Ok(());
        };
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            for (index, part) in parts.iter().enumerate() {
                if let Some(text) = part["text"].as_str() {
                    if part["thought"].as_bool() == Some(true) {
                        self.builder().thinking_delta(text);
                    } else {
                        self.builder().text_delta(text);
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let key = call["id"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| index.to_string());
                    self.builder().tool_call_start(
                        &key,
                        call["id"].as_str().unwrap_or(&key),
                        call["name"].as_str().unwrap_or(""),
                    );
                    self.builder().tool_call_delta(
                        &key,
                        &serde_json::to_string(&call["args"]).unwrap_or_else(|_| "{}".into()),
                    );
                }
            }
        }
        if candidate["finishReason"].is_string() {
            self.reason = common::stop_reason(candidate["finishReason"].as_str());
            self.finalize();
        }
        Ok(())
    }

    fn bedrock(&mut self, event: &Value) -> ApiResult<()> {
        if let Some(start) = event.get("contentBlockStart") {
            let index = start["contentBlockIndex"].as_u64().unwrap_or(0);
            let tool = &start["start"]["toolUse"];
            if tool.is_object() {
                let key = index.to_string();
                self.blocks.insert(index, format!("tool:{key}"));
                self.builder().tool_call_start(
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
                self.blocks.entry(index).or_insert_with(|| "text".into());
                self.builder().text_delta(text);
            }
            if let Some(text) = value
                .pointer("/reasoningContent/text")
                .and_then(Value::as_str)
            {
                self.blocks
                    .entry(index)
                    .or_insert_with(|| "thinking".into());
                self.builder().thinking_delta(text);
            }
            if let Some(args) = value.pointer("/toolUse/input").and_then(Value::as_str) {
                let key = self
                    .blocks
                    .get(&index)
                    .and_then(|kind| kind.strip_prefix("tool:"))
                    .map(str::to_owned)
                    .unwrap_or_else(|| index.to_string());
                self.builder().tool_call_delta(&key, args);
            }
        }
        if let Some(stop) = event.get("contentBlockStop") {
            let index = stop["contentBlockIndex"].as_u64().unwrap_or(0);
            match self.blocks.remove(&index).as_deref() {
                Some("text") => self.builder().end_text(),
                Some("thinking") => self.builder().end_thinking(),
                Some(kind) if kind.starts_with("tool:") => {
                    self.builder()
                        .end_tool_call(kind.trim_start_matches("tool:"));
                }
                _ => {}
            }
        }
        if let Some(stop) = event.get("messageStop") {
            self.reason = common::stop_reason(stop["stopReason"].as_str());
            self.bedrock_stopped = true;
        }
        if let Some(usage) = event.pointer("/metadata/usage") {
            self.builder().set_usage(
                usage["inputTokens"].as_u64(),
                usage["outputTokens"].as_u64(),
                usage["cacheReadInputTokens"].as_u64(),
                usage["cacheWriteInputTokens"].as_u64(),
                None,
            );
            if self.bedrock_stopped {
                self.finalize();
            }
        }
        if let Some(message) = event
            .pointer("/modelStreamErrorException/message")
            .and_then(Value::as_str)
        {
            return Err(message.to_owned());
        }
        Ok(())
    }
}

impl WireEventDecoder for JsonDecoder {
    fn initial_events(&mut self) -> Vec<AssistantMessageEvent> {
        self.builder().drain_events()
    }

    fn push_frame(&mut self, frame: &str) -> ApiResult<Vec<AssistantMessageEvent>> {
        if frame == "[DONE]" {
            if matches!(
                self.api.as_str(),
                "openai-completions" | "mistral-conversations"
            ) {
                self.finalize();
            }
            return Ok(self.builder().drain_events());
        }
        let event: Value = serde_json::from_str(frame)
            .map_err(|error| format!("invalid {} stream JSON: {error}", self.api))?;
        match self.api.as_str() {
            "anthropic-messages" => self.anthropic(&event)?,
            "openai-completions" => self.completions(&event, false),
            "mistral-conversations" => self.completions(&event, true),
            "openai-responses" | "openai-codex-responses" | "azure-openai-responses" => {
                self.responses(&event)?;
            }
            "google-generative-ai" | "google-vertex" => self.google(&event)?,
            "bedrock-converse-stream" => self.bedrock(&event)?,
            api => return Err(format!("unsupported incremental decoder for {api}")),
        }
        Ok(self.builder().drain_events())
    }

    fn finish(mut self: Box<Self>) -> ApiResult<Vec<AssistantMessageEvent>> {
        let reason = self.reason;
        let complete = self.complete;
        let mut builder = self.builder.take().expect("decoder is not finished");
        Ok(if complete {
            builder.drain_events()
        } else {
            builder.finish(reason)
        })
    }
}

struct PiDecoder {
    message: AssistantMessage,
    tool_json: HashMap<usize, String>,
    complete: bool,
}

impl PiDecoder {
    fn new(model: &Model) -> Self {
        Self {
            message: common::empty_message(model),
            tool_json: HashMap::new(),
            complete: false,
        }
    }

    fn set_content(&mut self, index: usize, content: Content) {
        while self.message.content.len() < index {
            self.message.content.push(Content::Text(TextContent {
                text: Default::default(),
                text_signature: None,
            }));
        }
        if index == self.message.content.len() {
            self.message.content.push(content);
        } else {
            self.message.content[index] = content;
        }
    }

    fn index(event: &Value) -> usize {
        event["contentIndex"].as_u64().unwrap_or(0) as usize
    }

    fn usage(&mut self, usage: &Value) {
        if let Ok(parsed) = serde_json::from_value::<Usage>(usage.clone()) {
            self.message.usage = parsed;
            return;
        }
        self.message.usage.input = usage["input"].as_u64().unwrap_or(0);
        self.message.usage.output = usage["output"].as_u64().unwrap_or(0);
        self.message.usage.cache_read = usage["cacheRead"].as_u64().unwrap_or(0);
        self.message.usage.cache_write = usage["cacheWrite"].as_u64().unwrap_or(0);
        self.message.usage.total_tokens = usage["totalTokens"].as_u64().unwrap_or(0);
    }
}

impl WireEventDecoder for PiDecoder {
    fn push_frame(&mut self, frame: &str) -> ApiResult<Vec<AssistantMessageEvent>> {
        let event: Value = serde_json::from_str(frame)
            .map_err(|error| format!("invalid pi-messages SSE JSON: {error}"))?;
        let mut events = Vec::new();
        match event["type"].as_str() {
            Some("start") => events.push(AssistantMessageEvent::Start {
                partial: self.message.clone(),
            }),
            Some("text_start") => {
                let index = Self::index(&event);
                self.set_content(
                    index,
                    Content::Text(TextContent {
                        text: Default::default(),
                        text_signature: None,
                    }),
                );
                events.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: self.message.clone(),
                });
            }
            Some("text_delta") => {
                let index = Self::index(&event);
                let delta = event["delta"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Text(text)) = self.message.content.get_mut(index) {
                    text.text = text.text.append(&delta);
                }
                events.push(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial: self.message.clone(),
                });
            }
            Some("text_end") => {
                let index = Self::index(&event);
                let content = event["content"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Text(text)) = self.message.content.get_mut(index) {
                    text.text = content.clone().into();
                    text.text_signature = event["contentSignature"].as_str().map(str::to_owned);
                }
                events.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: self.message.clone(),
                });
            }
            Some("thinking_start") => {
                let index = Self::index(&event);
                self.set_content(
                    index,
                    Content::Thinking(ThinkingContent {
                        thinking: Default::default(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                );
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: self.message.clone(),
                });
            }
            Some("thinking_delta") => {
                let index = Self::index(&event);
                let delta = event["delta"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Thinking(thinking)) = self.message.content.get_mut(index) {
                    thinking.thinking = thinking.thinking.append(&delta);
                }
                events.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial: self.message.clone(),
                });
            }
            Some("thinking_end") => {
                let index = Self::index(&event);
                let content = event["content"].as_str().unwrap_or("").to_owned();
                if let Some(Content::Thinking(thinking)) = self.message.content.get_mut(index) {
                    thinking.thinking = content.clone().into();
                    thinking.thinking_signature =
                        event["contentSignature"].as_str().map(str::to_owned);
                    thinking.redacted = event["redacted"].as_bool();
                }
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: self.message.clone(),
                });
            }
            Some("toolcall_start") => {
                let index = Self::index(&event);
                self.set_content(
                    index,
                    Content::ToolCall(ToolCall {
                        id: event["id"].as_str().unwrap_or("").to_owned(),
                        name: event["toolName"].as_str().unwrap_or("").to_owned(),
                        arguments: Map::new(),
                        thought_signature: None,
                    }),
                );
                self.tool_json.insert(index, String::new());
                events.push(AssistantMessageEvent::ToolcallStart {
                    content_index: index,
                    partial: self.message.clone(),
                });
            }
            Some("toolcall_delta") => {
                let index = Self::index(&event);
                let delta = event["delta"].as_str().unwrap_or("").to_owned();
                let json = self.tool_json.entry(index).or_default();
                json.push_str(&delta);
                if let Some(Content::ToolCall(call)) = self.message.content.get_mut(index)
                    && let Some(arguments) = parse_streaming_json(json).as_object()
                {
                    call.arguments = arguments.clone();
                }
                events.push(AssistantMessageEvent::ToolcallDelta {
                    content_index: index,
                    delta,
                    partial: self.message.clone(),
                });
            }
            Some("toolcall_end") => {
                let index = Self::index(&event);
                let tool_call = serde_json::from_value::<ToolCall>(event["toolCall"].clone())
                    .map_err(|error| format!("invalid pi tool call: {error}"))?;
                self.set_content(index, Content::ToolCall(tool_call.clone()));
                self.tool_json.remove(&index);
                events.push(AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call,
                    partial: self.message.clone(),
                });
            }
            Some("done") => {
                let reason = serde_json::from_value::<StopReason>(event["reason"].clone())
                    .unwrap_or(StopReason::Stop);
                self.message.stop_reason = reason;
                self.message.response_id = event["responseId"].as_str().map(str::to_owned);
                self.usage(&event["usage"]);
                self.complete = true;
                events.push(AssistantMessageEvent::Done {
                    reason,
                    message: self.message.clone(),
                });
            }
            Some("error") => {
                let reason = serde_json::from_value::<StopReason>(event["reason"].clone())
                    .unwrap_or(StopReason::Error);
                self.message.stop_reason = reason;
                self.message.error_message = event["errorMessage"].as_str().map(str::to_owned);
                self.usage(&event["usage"]);
                self.complete = true;
                events.push(AssistantMessageEvent::Error {
                    reason,
                    error: self.message.clone(),
                });
            }
            _ => {}
        }
        Ok(events)
    }

    fn finish(self: Box<Self>) -> ApiResult<Vec<AssistantMessageEvent>> {
        if self.complete {
            Ok(Vec::new())
        } else {
            Err("pi-messages stream ended without a terminal event".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, ModelCost, ModelInput};
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test Model".into(),
            api: Api::from("pi-messages"),
            provider: "test-provider".into(),
            base_url: "https://example.test".into(),
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
    fn pi_event_partials_keep_tool_argument_order() {
        let mut decoder = PiDecoder::new(&model());
        decoder
            .push_frame(
                &json!({
                    "type": "toolcall_start",
                    "contentIndex": 0,
                    "id": "call-ordered",
                    "toolName": "ordered"
                })
                .to_string(),
            )
            .unwrap();

        for (fragment, expected) in [
            (r#"{"z":1,"#, r#"{"z":1}"#),
            (r#""a":2,"#, r#"{"z":1,"a":2}"#),
            (r#""m":3}"#, r#"{"z":1,"a":2,"m":3}"#),
        ] {
            let events = decoder
                .push_frame(
                    &json!({
                        "type": "toolcall_delta",
                        "contentIndex": 0,
                        "delta": fragment
                    })
                    .to_string(),
                )
                .unwrap();
            let AssistantMessageEvent::ToolcallDelta { partial, .. } =
                events.last().expect("tool-call delta event")
            else {
                panic!("expected tool-call delta");
            };
            let Content::ToolCall(call) = &partial.content[0] else {
                panic!("expected tool call");
            };
            assert_eq!(serde_json::to_string(&call.arguments).unwrap(), expected);
        }
    }
}
