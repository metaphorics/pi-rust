//! API golden fixtures for all 10 built-in wire protocols.
//!
//! ## Fixture provenance
//!
//! | API | Request / stream fixture source |
//! |-----|----------------------------------|
//! | anthropic-messages | pi-test-derived (packages/ai stream shapes) + spec-shaped SSE |
//! | openai-completions | pi-test-derived SSE deltas |
//! | openai-responses | pi-test-derived Responses SSE |
//! | openai-codex-responses | pi-test-derived Responses SSE (same event mapping); transport extras (zstd/WS) covered by unit tests |
//! | azure-openai-responses | pi-test-derived Responses SSE |
//! | google-generative-ai | pi-test-derived generateContent SSE |
//! | google-vertex | pi-test-derived Vertex SSE |
//! | mistral-conversations | pi-test-derived completions-compatible SSE |
//! | bedrock-converse-stream | **spec-derived binary `application/vnd.amazon.eventstream` frames** encoded at test time from the JSONL event inventory (same converse-stream event names as the AWS SDK stream); JSONL file kept as the human-readable payload source |
//! | pi-messages | pi-test-derived custom SSE |
//!
//! Stream goldens assert **full event payloads** (types + text/thinking/tool deltas +
//! final stopReason/usage), not type-order alone.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream};
use parking_lot::Mutex;
use pi_ai::{
    api::{
        self, anthropic_messages, azure_openai_responses, bedrock_converse_stream,
        google_generative_ai, google_vertex, mistral_conversations, openai_codex_responses,
        openai_completions, openai_responses, pi_messages,
    },
    http::{HttpByteStream, HttpError, HttpFuture, StreamHttpClient},
    types::{
        Api, AssistantMessage, AssistantMessageEvent, Content, Context, Message, Model, ModelCost,
        ModelInput, StopReason, StreamOptions, TextContent, ThinkingContent, Tool, ToolCall,
        ToolResultMessage, Usage, UserContent, UserMessage,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/api");
const API_CASES: [(&str, &str); 10] = [
    ("anthropic-messages", "anthropic"),
    ("openai-completions", "openai_completions"),
    ("openai-responses", "openai_responses"),
    ("openai-codex-responses", "openai_codex_responses"),
    ("azure-openai-responses", "azure_openai_responses"),
    ("google-generative-ai", "google_generative_ai"),
    ("google-vertex", "google_vertex"),
    ("mistral-conversations", "mistral_conversations"),
    ("bedrock-converse-stream", "bedrock_converse_stream"),
    ("pi-messages", "pi_messages"),
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedEvents {
    types: Vec<String>,
    final_text: String,
    #[serde(default)]
    thinking_text: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_args_city: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage_input: Option<u64>,
    #[serde(default)]
    usage_output: Option<u64>,
    #[serde(default)]
    text_deltas: Option<Vec<String>>,
    #[serde(default)]
    thinking_deltas: Option<Vec<String>>,
    #[serde(default)]
    tool_deltas: Option<Vec<String>>,
}

fn model(api: &str) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: Api::from(api),
        provider: "test-provider".into(),
        base_url: "https://example.test/v1".into(),
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

fn context() -> Context {
    Context {
        system_prompt: Some("Be concise.".into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Hello".into()),
            timestamp: 0,
        })],
        tools: vec![Tool {
            name: "get_weather".into(),
            description: "Get weather".into(),
            parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
        }],
    }
}

fn tool_cycle_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![
            Message::Assistant(AssistantMessage {
                content: vec![
                    Content::Thinking(ThinkingContent {
                        thinking: "Prior thought".into(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                    Content::Text(TextContent {
                        text: "Calling".into(),
                        text_signature: None,
                    }),
                    Content::ToolCall(ToolCall {
                        id: "call-original|fc_item".into(),
                        name: "get_weather".into(),
                        arguments: HashMap::from([("city".into(), json!("Paris"))]),
                        thought_signature: None,
                    }),
                ],
                api: Api::from("openai-responses"),
                provider: "test-provider".into(),
                model: "test-model".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: 0,
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call-original|fc_item".into(),
                tool_name: "get_weather".into(),
                content: vec![Content::Text(TextContent {
                    text: "Sunny".into(),
                    text_signature: None,
                })],
                details: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 0,
            }),
        ],
        tools: Vec::new(),
    }
}

fn options() -> StreamOptions {
    StreamOptions {
        temperature: Some(0.2),
        max_tokens: Some(64),
        api_key: Some("test-key".into()),
        ..Default::default()
    }
}

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(format!("{FIXTURES}/{path}")).unwrap()
}
fn fixture_json(path: &str) -> Value {
    serde_json::from_slice(&fixture(path)).unwrap()
}

fn build_request(api: &str, model: &Model, context: &Context, options: &StreamOptions) -> Value {
    match api {
        "anthropic-messages" => anthropic_messages::build_request_body(model, context, options),
        "openai-completions" => openai_completions::build_request_body(model, context, options),
        "openai-responses" => openai_responses::build_request_body(model, context, options),
        "openai-codex-responses" => {
            openai_codex_responses::build_request_body(model, context, options)
        }
        "azure-openai-responses" => {
            azure_openai_responses::build_request_body(model, context, options)
        }
        "google-generative-ai" => google_generative_ai::build_request_body(model, context, options),
        "google-vertex" => google_vertex::build_request_body(model, context, options),
        "mistral-conversations" => {
            mistral_conversations::build_request_body(model, context, options)
        }
        "bedrock-converse-stream" => {
            bedrock_converse_stream::build_request_body(model, context, options)
        }
        "pi-messages" => pi_messages::build_request_body(model, context, options),
        _ => unreachable!(),
    }
}

fn stream_fixture_bytes(api: &str, fixture_name: &str) -> Vec<u8> {
    if api == "bedrock-converse-stream" {
        // Spec-derived binary eventstream: encode real frames from the JSONL inventory.
        let jsonl = fixture(&format!("stream_{fixture_name}.jsonl"));
        bedrock_converse_stream::encode_jsonl_as_eventstream(&jsonl).expect("encode eventstream")
    } else {
        fixture(&format!("stream_{fixture_name}.sse"))
    }
}

fn parse_events(api: &str, bytes: &[u8], model: &Model) -> Vec<AssistantMessageEvent> {
    let result = match api {
        "anthropic-messages" => anthropic_messages::parse_stream_events([bytes], model),
        "openai-completions" => openai_completions::parse_stream_events([bytes], model),
        "openai-responses" => openai_responses::parse_stream_events([bytes], model),
        "openai-codex-responses" => openai_codex_responses::parse_stream_events([bytes], model),
        "azure-openai-responses" => azure_openai_responses::parse_stream_events([bytes], model),
        "google-generative-ai" => google_generative_ai::parse_stream_events([bytes], model),
        "google-vertex" => google_vertex::parse_stream_events([bytes], model),
        "mistral-conversations" => mistral_conversations::parse_stream_events([bytes], model),
        "bedrock-converse-stream" => bedrock_converse_stream::parse_stream_events([bytes], model),
        "pi-messages" => pi_messages::parse_stream_events([bytes], model),
        _ => unreachable!(),
    };
    result.unwrap()
}

fn event_type(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

fn final_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.text.as_string()),
            _ => None,
        })
        .collect()
}

fn final_thinking(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Thinking(thinking) => Some(thinking.thinking.as_string()),
            _ => None,
        })
        .collect()
}

fn text_deltas(events: &[AssistantMessageEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect()
}

fn thinking_deltas(events: &[AssistantMessageEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect()
}

fn tool_deltas(events: &[AssistantMessageEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect()
}

fn assert_event_payloads(api: &str, events: &[AssistantMessageEvent], expected: &ExpectedEvents) {
    assert_eq!(
        events.iter().map(event_type).collect::<Vec<_>>(),
        expected.types,
        "{api} event types"
    );

    if let Some(deltas) = &expected.text_deltas {
        assert_eq!(text_deltas(events), *deltas, "{api} text deltas");
    }
    if let Some(deltas) = &expected.thinking_deltas {
        assert_eq!(thinking_deltas(events), *deltas, "{api} thinking deltas");
    }
    if let Some(deltas) = &expected.tool_deltas {
        assert_eq!(tool_deltas(events), *deltas, "{api} tool deltas");
    }

    let final_message = events
        .last()
        .and_then(AssistantMessageEvent::final_message)
        .unwrap_or_else(|| panic!("{api}: missing final message"));
    assert_eq!(final_text(final_message), expected.final_text, "{api} finalText");

    if let Some(thinking) = &expected.thinking_text {
        assert_eq!(final_thinking(final_message), *thinking, "{api} thinking");
    }

    if let Some(name) = &expected.tool_name {
        let tool = final_message
            .content
            .iter()
            .find_map(|c| match c {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{api}: missing tool call"));
        assert_eq!(&tool.name, name, "{api} tool name");
        if let Some(city) = &expected.tool_args_city {
            assert_eq!(
                tool.arguments.get("city").and_then(Value::as_str),
                Some(city.as_str()),
                "{api} tool city"
            );
        }
    }

    if let Some(reason) = &expected.stop_reason {
        let actual = match final_message.stop_reason {
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolUse => "toolUse",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
        };
        assert_eq!(actual, reason, "{api} stopReason");
    }
    if let Some(input) = expected.usage_input {
        assert_eq!(final_message.usage.input, input, "{api} usage.input");
    }
    if let Some(output) = expected.usage_output {
        assert_eq!(final_message.usage.output, output, "{api} usage.output");
    }
}

#[test]
fn request_bodies_match_golden_fixtures() {
    for (api, fixture_name) in API_CASES {
        let actual = build_request(api, &model(api), &context(), &options());
        assert_eq!(
            actual,
            fixture_json(&format!("request_{fixture_name}.json")),
            "{api}"
        );
    }
}

#[test]
fn provider_specific_request_compatibility_matches_goldens() {
    let context = context();
    let options = options();
    let mut oauth_options = options.clone();
    oauth_options.api_key = Some("sk-ant-oat-test".into());
    assert_eq!(
        anthropic_messages::build_request_body(
            &model("anthropic-messages"),
            &context,
            &oauth_options
        ),
        fixture_json("request_anthropic_oauth.json")
    );
    let oauth_headers: HashMap<_, _> =
        anthropic_messages::build_headers(&model("anthropic-messages"), &oauth_options)
            .into_iter()
            .collect();
    assert_eq!(
        serde_json::to_value(oauth_headers).unwrap(),
        fixture_json("headers_anthropic_oauth.json")
    );

    let mut openai_model = model("openai-completions");
    openai_model.reasoning = true;
    openai_model.compat = Some(json!({
        "maxTokensField":"max_tokens",
        "thinkingFormat":"openrouter",
        "supportsUsageInStreaming":false
    }));
    assert_eq!(
        openai_completions::build_request_body(&openai_model, &context, &options),
        fixture_json("request_openai_completions_compat.json")
    );

    let tool_context = tool_cycle_context();
    assert_eq!(
        openai_responses::build_request_body(&model("openai-responses"), &tool_context, &options),
        fixture_json("request_openai_responses_tool_cycle.json")
    );
    assert_eq!(
        mistral_conversations::build_request_body(
            &model("mistral-conversations"),
            &tool_context,
            &options
        ),
        fixture_json("request_mistral_conversations_tool_cycle.json")
    );
}

#[test]
fn stream_parsers_match_golden_event_payloads() {
    for (api, fixture_name) in API_CASES {
        let bytes = stream_fixture_bytes(api, fixture_name);
        if api == "bedrock-converse-stream" {
            assert_ne!(bytes.first().copied(), Some(b'{'), "bedrock must be binary");
        }
        let events = parse_events(api, &bytes, &model(api));
        let expected: ExpectedEvents = serde_json::from_value(fixture_json(&format!(
            "expected_events_{fixture_name}.json"
        )))
        .unwrap();
        assert_event_payloads(api, &events, &expected);
    }
}

#[test]
fn bedrock_eventstream_roundtrip_from_jsonl_source() {
    let jsonl = fixture("stream_bedrock_converse_stream.jsonl");
    let binary = bedrock_converse_stream::encode_jsonl_as_eventstream(&jsonl).unwrap();
    let from_binary =
        parse_events("bedrock-converse-stream", &binary, &model("bedrock-converse-stream"));
    let from_jsonl =
        parse_events("bedrock-converse-stream", &jsonl, &model("bedrock-converse-stream"));
    assert_eq!(
        from_binary.iter().map(event_type).collect::<Vec<_>>(),
        from_jsonl.iter().map(event_type).collect::<Vec<_>>()
    );
    assert_eq!(
        text_deltas(&from_binary),
        text_deltas(&from_jsonl)
    );
}

#[derive(Clone)]
struct MockHttp {
    chunks: Vec<Vec<u8>>,
}

impl StreamHttpClient for MockHttp {
    fn post_sse<'a>(
        &'a self,
        _url: &'a str,
        _headers: &'a [(String, String)],
        _body: &'a Value,
    ) -> HttpFuture<'a> {
        self.response()
    }
    fn post_json_stream<'a>(
        &'a self,
        _url: &'a str,
        _headers: &'a [(String, String)],
        _body: &'a Value,
    ) -> HttpFuture<'a> {
        self.response()
    }
    fn post_bytes<'a>(
        &'a self,
        _url: &'a str,
        _headers: &'a [(String, String)],
        _body: &'a [u8],
    ) -> HttpFuture<'a> {
        self.response()
    }
}

impl MockHttp {
    fn response(&self) -> HttpFuture<'_> {
        let chunks = self.chunks.clone();
        Box::pin(async move {
            let body: HttpByteStream =
                Box::pin(stream::iter(chunks.into_iter().map(Ok::<_, HttpError>)));
            Ok(body)
        })
    }
}

type HttpChunkReceiver = tokio::sync::mpsc::Receiver<Result<Vec<u8>, HttpError>>;

struct GatedHttp {
    receiver: Mutex<Option<HttpChunkReceiver>>,
}

impl StreamHttpClient for GatedHttp {
    fn post_sse<'a>(
        &'a self,
        _url: &'a str,
        _headers: &'a [(String, String)],
        _body: &'a Value,
    ) -> HttpFuture<'a> {
        let receiver = self.receiver.lock().take().expect("single request");
        Box::pin(async move {
            let body: HttpByteStream = Box::pin(stream::unfold(receiver, |mut receiver| async {
                receiver.recv().await.map(|item| (item, receiver))
            }));
            Ok(body)
        })
    }

    fn post_json_stream<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a Value,
    ) -> HttpFuture<'a> {
        self.post_sse(url, headers, body)
    }

    fn post_bytes<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        _body: &'a [u8],
    ) -> HttpFuture<'a> {
        self.post_sse(url, headers, &Value::Null)
    }
}

#[tokio::test]
async fn emits_provider_deltas_before_http_eof() {
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    let client = Arc::new(GatedHttp {
        receiver: Mutex::new(Some(receiver)),
    });
    let mut events = api::stream_dispatch_with_client(
        "openai-completions",
        model("openai-completions"),
        context(),
        options(),
        client,
    );

    let start = tokio::time::timeout(Duration::from_millis(250), events.next())
        .await
        .expect("start must arrive while HTTP remains open")
        .unwrap();
    assert_eq!(event_type(&start), "start");

    sender
        .send(Ok(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n"
                .to_vec(),
        ))
        .await
        .unwrap();
    let text_start = tokio::time::timeout(Duration::from_millis(250), events.next())
        .await
        .expect("text start must arrive before EOF")
        .unwrap();
    let text_delta = tokio::time::timeout(Duration::from_millis(250), events.next())
        .await
        .expect("text delta must arrive before EOF")
        .unwrap();
    assert_eq!(event_type(&text_start), "text_start");
    assert_eq!(event_type(&text_delta), "text_delta");
    if let AssistantMessageEvent::TextDelta { delta, .. } = &text_delta {
        assert_eq!(delta, "Hi");
    }

    drop(sender);
    while events.next().await.is_some() {}
}

#[tokio::test]
async fn every_builtin_dispatches_through_injected_http() {
    assert_eq!(api::BUILTIN_APIS.len(), 10);
    for (api_id, fixture_name) in API_CASES {
        let bytes = stream_fixture_bytes(api_id, fixture_name);
        let split = bytes.len() / 2;
        let client = Arc::new(MockHttp {
            chunks: vec![bytes[..split].to_vec(), bytes[split..].to_vec()],
        });
        let mut events =
            api::stream_dispatch_with_client(api_id, model(api_id), context(), options(), client);
        let mut final_message = None;
        let mut collected = Vec::new();
        while let Some(event) = events.next().await {
            if let Some(message) = event.final_message() {
                final_message = Some(message.clone());
            }
            collected.push(event);
        }
        assert_eq!(final_text(&final_message.unwrap()), "Hello", "{api_id}");
        assert!(
            text_deltas(&collected).iter().any(|d| d.contains('H') || d == "Hello" || d == "Hi"
                || !d.is_empty()),
            "{api_id} should emit text deltas"
        );
    }
}

#[test]
fn codex_zstd_header_when_compressing() {
    let json = serde_json::to_vec(&json!({"model":"x","input":[]})).unwrap();
    let compressed = openai_codex_responses::compress_request_body_zstd(&json).unwrap();
    assert_ne!(compressed, json);
    assert!(openai_codex_responses::should_fallback_to_sse(
        "websocket connect: failed",
        false
    ));
}
