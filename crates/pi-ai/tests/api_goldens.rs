use std::{collections::HashMap, sync::Arc};

use futures_util::{StreamExt, stream};
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
            Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
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
fn stream_parsers_match_golden_event_sequences() {
    for (api, fixture_name) in API_CASES {
        let extension = if api == "bedrock-converse-stream" {
            "jsonl"
        } else {
            "sse"
        };
        let events = parse_events(
            api,
            &fixture(&format!("stream_{fixture_name}.{extension}")),
            &model(api),
        );
        let expected: ExpectedEvents = serde_json::from_value(fixture_json(&format!(
            "expected_events_{fixture_name}.json"
        )))
        .unwrap();
        assert_eq!(
            events.iter().map(event_type).collect::<Vec<_>>(),
            expected.types,
            "{api}"
        );
        let final_message = events
            .last()
            .and_then(AssistantMessageEvent::final_message)
            .unwrap();
        assert_eq!(final_text(final_message), expected.final_text, "{api}");
    }
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

#[tokio::test]
async fn every_builtin_dispatches_through_injected_http() {
    assert_eq!(api::BUILTIN_APIS.len(), 10);
    for (api_id, fixture_name) in API_CASES {
        let extension = if api_id == "bedrock-converse-stream" {
            "jsonl"
        } else {
            "sse"
        };
        let bytes = fixture(&format!("stream_{fixture_name}.{extension}"));
        let split = bytes.len() / 2;
        let client = Arc::new(MockHttp {
            chunks: vec![bytes[..split].to_vec(), bytes[split..].to_vec()],
        });
        let mut events =
            api::stream_dispatch_with_client(api_id, model(api_id), context(), options(), client);
        let mut final_message = None;
        while let Some(event) = events.next().await {
            if let Some(message) = event.final_message() {
                final_message = Some(message.clone());
            }
        }
        assert_eq!(final_text(&final_message.unwrap()), "Hello", "{api_id}");
    }
}
