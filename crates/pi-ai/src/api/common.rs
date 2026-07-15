use std::{collections::HashMap, sync::Arc, time::{SystemTime, UNIX_EPOCH}};

use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    event_stream::AssistantMessageEventStream,
    http::StreamHttpClient,
    json_parse::parse_streaming_json,
    types::{AssistantMessage, AssistantMessageEvent, Content, Model, StopReason, TextContent, ThinkingContent, ToolCall, Usage},
};

pub type ApiResult<T> = Result<T, String>;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub fn empty_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    }
}

pub struct EventBuilder {
    pub message: AssistantMessage,
    events: Vec<AssistantMessageEvent>,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    tool_indexes: HashMap<String, usize>,
    tool_arguments: HashMap<String, String>,
}

impl EventBuilder {
    pub fn new(model: &Model) -> Self {
        let message = empty_message(model);
        Self {
            events: vec![AssistantMessageEvent::Start { partial: message.clone() }],
            message,
            text_index: None,
            thinking_index: None,
            tool_indexes: HashMap::new(),
            tool_arguments: HashMap::new(),
        }
    }

    pub fn set_response_id(&mut self, id: Option<&str>) { self.message.response_id = id.map(str::to_owned); }
    pub fn set_response_model(&mut self, id: Option<&str>) { self.message.response_model = id.map(str::to_owned); }

    pub fn text_delta(&mut self, delta: &str) {
        if delta.is_empty() { return; }
        let index = *self.text_index.get_or_insert_with(|| {
            let index = self.message.content.len();
            self.message.content.push(Content::Text(TextContent { text: String::new(), text_signature: None }));
            self.events.push(AssistantMessageEvent::TextStart { content_index: index, partial: self.message.clone() });
            index
        });
        if let Content::Text(text) = &mut self.message.content[index] { text.text.push_str(delta); }
        self.events.push(AssistantMessageEvent::TextDelta { content_index: index, delta: delta.to_owned(), partial: self.message.clone() });
    }

    pub fn thinking_delta(&mut self, delta: &str) {
        if delta.is_empty() { return; }
        let index = *self.thinking_index.get_or_insert_with(|| {
            let index = self.message.content.len();
            self.message.content.push(Content::Thinking(ThinkingContent { thinking: String::new(), thinking_signature: None, redacted: None }));
            self.events.push(AssistantMessageEvent::ThinkingStart { content_index: index, partial: self.message.clone() });
            index
        });
        if let Content::Thinking(thinking) = &mut self.message.content[index] { thinking.thinking.push_str(delta); }
        self.events.push(AssistantMessageEvent::ThinkingDelta { content_index: index, delta: delta.to_owned(), partial: self.message.clone() });
    }

    pub fn set_thinking_signature(&mut self, signature: String) {
        if let Some(index) = self.thinking_index {
            if let Content::Thinking(thinking) = &mut self.message.content[index] { thinking.thinking_signature = Some(signature); }
        }
    }

    pub fn tool_call_start(&mut self, key: &str, id: &str, name: &str) {
        if self.tool_indexes.contains_key(key) { return; }
        let index = self.message.content.len();
        self.message.content.push(Content::ToolCall(ToolCall { id: id.to_owned(), name: name.to_owned(), arguments: HashMap::new(), thought_signature: None }));
        self.tool_indexes.insert(key.to_owned(), index);
        self.tool_arguments.insert(key.to_owned(), String::new());
        self.events.push(AssistantMessageEvent::ToolcallStart { content_index: index, partial: self.message.clone() });
    }

    pub fn tool_call_delta(&mut self, key: &str, delta: &str) {
        let Some(&index) = self.tool_indexes.get(key) else { return; };
        self.tool_arguments.entry(key.to_owned()).or_default().push_str(delta);
        let parsed = parse_streaming_json(self.tool_arguments.get(key).map_or("", String::as_str));
        if let (Content::ToolCall(call), Some(args)) = (&mut self.message.content[index], parsed.as_object()) {
            call.arguments = args.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        self.events.push(AssistantMessageEvent::ToolcallDelta { content_index: index, delta: delta.to_owned(), partial: self.message.clone() });
    }

    pub fn set_tool_signature(&mut self, key: &str, signature: String) {
        if let Some(&index) = self.tool_indexes.get(key) {
            if let Content::ToolCall(call) = &mut self.message.content[index] { call.thought_signature = Some(signature); }
        }
    }

    pub fn set_usage(&mut self, input: Option<u64>, output: Option<u64>, cache_read: Option<u64>, cache_write: Option<u64>, reasoning: Option<u64>) {
        if let Some(value) = input { self.message.usage.input = value; }
        if let Some(value) = output { self.message.usage.output = value; }
        if let Some(value) = cache_read { self.message.usage.cache_read = value; }
        if let Some(value) = cache_write { self.message.usage.cache_write = value; }
        self.message.usage.reasoning = reasoning.or(self.message.usage.reasoning);
        self.message.usage.total_tokens = self.message.usage.input + self.message.usage.output + self.message.usage.cache_read + self.message.usage.cache_write;
    }

    pub fn finish(mut self, reason: StopReason) -> Vec<AssistantMessageEvent> {
        if let Some(index) = self.text_index {
            let text = match &self.message.content[index] { Content::Text(text) => text.text.clone(), _ => String::new() };
            self.events.push(AssistantMessageEvent::TextEnd { content_index: index, content: text, partial: self.message.clone() });
        }
        if let Some(index) = self.thinking_index {
            let content = match &self.message.content[index] { Content::Thinking(value) => value.thinking.clone(), _ => String::new() };
            self.events.push(AssistantMessageEvent::ThinkingEnd { content_index: index, content, partial: self.message.clone() });
        }
        let mut tools: Vec<_> = self.tool_indexes.into_iter().collect();
        tools.sort_by_key(|(_, index)| *index);
        for (_, index) in tools {
            if let Content::ToolCall(call) = self.message.content[index].clone() {
                self.events.push(AssistantMessageEvent::ToolcallEnd { content_index: index, tool_call: call, partial: self.message.clone() });
            }
        }
        self.message.stop_reason = reason;
        self.events.push(AssistantMessageEvent::Done { reason, message: self.message });
        self.events
    }
}

pub fn stop_reason(value: Option<&str>) -> StopReason {
    match value {
        Some("length" | "max_tokens" | "MAX_TOKENS" | "incomplete") => StopReason::Length,
        Some("tool_calls" | "tool_use" | "function_call" | "TOOL_USE") => StopReason::ToolUse,
        Some("error" | "failed" | "cancelled") => StopReason::Error,
        _ => StopReason::Stop,
    }
}


pub fn merged_headers(model: &Model, options: &crate::types::StreamOptions) -> Vec<(String, String)> {
    let mut result: HashMap<String, String> = model.headers.clone().unwrap_or_default();
    if let Some(headers) = &options.headers {
        for (name, value) in headers {
            if let Some(value) = value { result.insert(name.clone(), value.clone()); } else { result.remove(name); }
        }
    }
    result.into_iter().collect()
}

pub fn spawn_stream<F>(
    model: Model,
    context: crate::types::Context,
    options: crate::types::StreamOptions,
    client: Arc<dyn StreamHttpClient>,
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
    parser: F,
    json_stream: bool,
) -> AssistantMessageEventStream
where
    F: Fn(Vec<Vec<u8>>, &Model) -> ApiResult<Vec<AssistantMessageEvent>> + Send + Sync + 'static,
{
    let stream = AssistantMessageEventStream::new();
    let producer = stream.clone();
    tokio::spawn(async move {
        let result = async {
            let mut response = if json_stream {
                client.post_json_stream(&url, &headers, &body).await
            } else {
                client.post_sse(&url, &headers, &body).await
            }
            .map_err(|error| error.to_string())?;
            let mut chunks = Vec::new();
            while let Some(chunk) = response.next().await {
                chunks.push(chunk.map_err(|error| error.to_string())?);
            }
            parser(chunks, &model)
        }
        .await;
        match result {
            Ok(events) => {
                for event in events {
                    producer.push(event);
                }
            }
            Err(error) => {
                let mut message = empty_message(&model);
                message.stop_reason = StopReason::Error;
                message.error_message = Some(error);
                producer.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: message,
                });
            }
        }
        producer.end(None);
        drop((context, options));
    });
    stream
}
