use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    event_stream::AssistantMessageEventStream,
    http::StreamHttpClient,
    json_parse::parse_streaming_json,
    sse::SseParser,
    types::{
        AssistantMessage, AssistantMessageEvent, Content, Model, StopReason, TextContent,
        ThinkingContent, ToolCall, Usage,
    },
};

pub type ApiResult<T> = Result<T, String>;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
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
            events: vec![AssistantMessageEvent::Start {
                partial: message.clone(),
            }],
            message,
            text_index: None,
            thinking_index: None,
            tool_indexes: HashMap::new(),
            tool_arguments: HashMap::new(),
        }
    }

    pub fn set_response_id(&mut self, id: Option<&str>) {
        self.message.response_id = id.map(str::to_owned);
    }
    pub fn set_response_model(&mut self, id: Option<&str>) {
        self.message.response_model = id.map(str::to_owned);
    }

    pub fn text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let index = *self.text_index.get_or_insert_with(|| {
            let index = self.message.content.len();
            self.message.content.push(Content::Text(TextContent {
                text: Default::default(),
                text_signature: None,
            }));
            self.events.push(AssistantMessageEvent::TextStart {
                content_index: index,
                partial: self.message.clone(),
            });
            index
        });
        if let Content::Text(text) = &mut self.message.content[index] {
            text.text = text.text.append(delta);
        }
        self.events.push(AssistantMessageEvent::TextDelta {
            content_index: index,
            delta: delta.to_owned(),
            partial: self.message.clone(),
        });
    }

    pub fn thinking_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let index = *self.thinking_index.get_or_insert_with(|| {
            let index = self.message.content.len();
            self.message
                .content
                .push(Content::Thinking(ThinkingContent {
                    thinking: Default::default(),
                    thinking_signature: None,
                    redacted: None,
                }));
            self.events.push(AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial: self.message.clone(),
            });
            index
        });
        if let Content::Thinking(thinking) = &mut self.message.content[index] {
            thinking.thinking = thinking.thinking.append(delta);
        }
        self.events.push(AssistantMessageEvent::ThinkingDelta {
            content_index: index,
            delta: delta.to_owned(),
            partial: self.message.clone(),
        });
    }

    pub fn set_thinking_signature(&mut self, signature: String) {
        if let Some(index) = self.thinking_index
            && let Content::Thinking(thinking) = &mut self.message.content[index]
        {
            thinking.thinking_signature = Some(signature);
        }
    }

    pub fn tool_call_start(&mut self, key: &str, id: &str, name: &str) {
        if self.tool_indexes.contains_key(key) {
            return;
        }
        let index = self.message.content.len();
        self.message.content.push(Content::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: HashMap::new(),
            thought_signature: None,
        }));
        self.tool_indexes.insert(key.to_owned(), index);
        self.tool_arguments.insert(key.to_owned(), String::new());
        self.events.push(AssistantMessageEvent::ToolcallStart {
            content_index: index,
            partial: self.message.clone(),
        });
    }

    pub fn tool_call_delta(&mut self, key: &str, delta: &str) {
        let Some(&index) = self.tool_indexes.get(key) else {
            return;
        };
        self.tool_arguments
            .entry(key.to_owned())
            .or_default()
            .push_str(delta);
        let parsed = parse_streaming_json(self.tool_arguments.get(key).map_or("", String::as_str));
        if let (Content::ToolCall(call), Some(args)) =
            (&mut self.message.content[index], parsed.as_object())
        {
            call.arguments = args.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        self.events.push(AssistantMessageEvent::ToolcallDelta {
            content_index: index,
            delta: delta.to_owned(),
            partial: self.message.clone(),
        });
    }

    pub fn set_tool_signature(&mut self, key: &str, signature: String) {
        if let Some(&index) = self.tool_indexes.get(key)
            && let Content::ToolCall(call) = &mut self.message.content[index]
        {
            call.thought_signature = Some(signature);
        }
    }

    pub fn end_tool_call(&mut self, key: &str) {
        let Some(index) = self.tool_indexes.remove(key) else {
            return;
        };
        self.tool_arguments.remove(key);
        if let Content::ToolCall(tool_call) = self.message.content[index].clone() {
            self.events.push(AssistantMessageEvent::ToolcallEnd {
                content_index: index,
                tool_call,
                partial: self.message.clone(),
            });
        }
    }

    pub fn end_text(&mut self) {
        let Some(index) = self.text_index.take() else {
            return;
        };
        let content = match &self.message.content[index] {
            Content::Text(text) => text.text.as_string(),
            _ => String::new(),
        };
        self.events.push(AssistantMessageEvent::TextEnd {
            content_index: index,
            content,
            partial: self.message.clone(),
        });
    }

    pub fn end_thinking(&mut self) {
        let Some(index) = self.thinking_index.take() else {
            return;
        };
        let content = match &self.message.content[index] {
            Content::Thinking(thinking) => thinking.thinking.as_string(),
            _ => String::new(),
        };
        self.events.push(AssistantMessageEvent::ThinkingEnd {
            content_index: index,
            content,
            partial: self.message.clone(),
        });
    }

    pub fn drain_events(&mut self) -> Vec<AssistantMessageEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn set_usage(
        &mut self,
        input: Option<u64>,
        output: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
        reasoning: Option<u64>,
    ) {
        if let Some(value) = input {
            self.message.usage.input = value;
        }
        if let Some(value) = output {
            self.message.usage.output = value;
        }
        if let Some(value) = cache_read {
            self.message.usage.cache_read = value;
        }
        if let Some(value) = cache_write {
            self.message.usage.cache_write = value;
        }
        self.message.usage.reasoning = reasoning.or(self.message.usage.reasoning);
        self.message.usage.total_tokens = self.message.usage.input
            + self.message.usage.output
            + self.message.usage.cache_read
            + self.message.usage.cache_write;
    }

    pub fn finalize(&mut self, reason: StopReason) {
        self.end_text();
        self.end_thinking();
        let mut tools: Vec<_> = self
            .tool_indexes
            .iter()
            .map(|(key, index)| (key.clone(), *index))
            .collect();
        tools.sort_by_key(|(_, index)| *index);
        for (key, _) in tools {
            self.end_tool_call(&key);
        }
        self.message.stop_reason = reason;
        self.events.push(AssistantMessageEvent::Done {
            reason,
            message: self.message.clone(),
        });
    }

    pub fn finish(mut self, reason: StopReason) -> Vec<AssistantMessageEvent> {
        self.finalize(reason);
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

pub fn merged_headers(
    model: &Model,
    options: &crate::types::StreamOptions,
) -> Vec<(String, String)> {
    let mut result: HashMap<String, String> = model.headers.clone().unwrap_or_default();
    if let Some(headers) = &options.headers {
        for (name, value) in headers {
            if let Some(value) = value {
                result.insert(name.clone(), value.clone());
            } else {
                result.remove(name);
            }
        }
    }
    result.into_iter().collect()
}

pub trait WireEventDecoder: Send {
    fn initial_events(&mut self) -> Vec<AssistantMessageEvent> {
        Vec::new()
    }
    fn push_frame(&mut self, frame: &str) -> ApiResult<Vec<AssistantMessageEvent>>;
    fn finish(self: Box<Self>) -> ApiResult<Vec<AssistantMessageEvent>>;
}

#[derive(Default)]
pub(crate) struct JsonLineParser {
    pending: Vec<u8>,
}

impl JsonLineParser {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.is_empty() {
                lines.push(String::from_utf8_lossy(&line).into_owned());
            }
        }
        lines
    }

    pub(crate) fn finish(mut self) -> Vec<String> {
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()]
        }
    }
}

enum FrameParser {
    Sse(SseParser),
    JsonLines(JsonLineParser),
}

impl FrameParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        match self {
            Self::Sse(parser) => parser.push(bytes),
            Self::JsonLines(parser) => parser.push(bytes),
        }
    }

    fn finish(self) -> Vec<String> {
        match self {
            Self::Sse(parser) => parser.finish(),
            Self::JsonLines(parser) => parser.finish(),
        }
    }
}

pub fn decode_sse_chunks<I, B>(
    chunks: I,
    mut decoder: Box<dyn WireEventDecoder>,
) -> ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut events = decoder.initial_events();
    for frame in crate::sse::parse_sse_chunks(chunks) {
        events.extend(decoder.push_frame(&frame)?);
    }
    events.extend(decoder.finish()?);
    Ok(events)
}

pub fn decode_json_chunks<I, B>(
    chunks: I,
    mut decoder: Box<dyn WireEventDecoder>,
) -> ApiResult<Vec<AssistantMessageEvent>>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut parser = JsonLineParser::default();
    let mut events = decoder.initial_events();
    for chunk in chunks {
        for frame in parser.push(chunk.as_ref()) {
            events.extend(decoder.push_frame(&frame)?);
        }
    }
    for frame in parser.finish() {
        events.extend(decoder.push_frame(&frame)?);
    }
    events.extend(decoder.finish()?);
    Ok(events)
}

pub struct WireRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub json_stream: bool,
}

pub fn spawn_stream(
    model: Model,
    client: Arc<dyn StreamHttpClient>,
    request: WireRequest,
) -> AssistantMessageEventStream {
    let mut decoder = super::incremental::decoder(&model);
    let stream = AssistantMessageEventStream::new();
    let producer = stream.clone();
    tokio::spawn(async move {
        let result: ApiResult<()> = async {
            let mut response = if request.json_stream {
                client
                    .post_json_stream(&request.url, &request.headers, &request.body)
                    .await
            } else {
                client
                    .post_sse(&request.url, &request.headers, &request.body)
                    .await
            }
            .map_err(|error| error.to_string())?;
            let mut frames = if request.json_stream {
                FrameParser::JsonLines(JsonLineParser::default())
            } else {
                FrameParser::Sse(SseParser::default())
            };
            for event in decoder.initial_events() {
                producer.push(event);
            }
            while let Some(chunk) = response.next().await {
                let chunk = chunk.map_err(|error| error.to_string())?;
                for frame in frames.push(&chunk) {
                    for event in decoder.push_frame(&frame)? {
                        producer.push(event);
                    }
                }
            }
            for frame in frames.finish() {
                for event in decoder.push_frame(&frame)? {
                    producer.push(event);
                }
            }
            for event in decoder.finish()? {
                producer.push(event);
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let mut message = empty_message(&model);
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
