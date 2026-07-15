use std::sync::Arc;

use crate::{
    event_stream::AssistantMessageEventStream,
    http::StreamHttpClient,
    types::{AssistantMessageEvent, Context, Model, StopReason, StreamOptions},
};

mod common;
mod incremental;
pub use common::EventBuilder;

pub mod anthropic_messages;
pub mod azure_openai_responses;
pub mod bedrock_converse_stream;
pub mod google_generative_ai;
pub mod google_shared;
pub mod google_vertex;
pub mod mistral_conversations;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;
pub mod openai_responses_shared;
pub mod pi_messages;
pub mod simple_options;
pub mod transform_messages;

pub const BUILTIN_APIS: [&str; 10] = [
    "anthropic-messages",
    "openai-completions",
    "openai-responses",
    "openai-codex-responses",
    "azure-openai-responses",
    "google-generative-ai",
    "google-vertex",
    "mistral-conversations",
    "bedrock-converse-stream",
    "pi-messages",
];

pub fn stream_dispatch(
    api: &str,
    model: Model,
    context: Context,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    match api {
        "anthropic-messages" => anthropic_messages::stream(model, context, options),
        "openai-completions" => openai_completions::stream(model, context, options),
        "openai-responses" => openai_responses::stream(model, context, options),
        "openai-codex-responses" => openai_codex_responses::stream(model, context, options),
        "azure-openai-responses" => azure_openai_responses::stream(model, context, options),
        "google-generative-ai" => google_generative_ai::stream(model, context, options),
        "google-vertex" => google_vertex::stream(model, context, options),
        "mistral-conversations" => mistral_conversations::stream(model, context, options),
        "bedrock-converse-stream" => bedrock_converse_stream::stream(model, context, options),
        "pi-messages" => pi_messages::stream(model, context, options),
        _ => unsupported_stream(model, format!("unknown API {api:?}")),
    }
}

pub fn stream_dispatch_with_client(
    api: &str,
    model: Model,
    context: Context,
    options: StreamOptions,
    client: Arc<dyn StreamHttpClient>,
) -> AssistantMessageEventStream {
    match api {
        "anthropic-messages" => {
            anthropic_messages::stream_with_client(model, context, options, client)
        }
        "openai-completions" => {
            openai_completions::stream_with_client(model, context, options, client)
        }
        "openai-responses" => openai_responses::stream_with_client(model, context, options, client),
        "openai-codex-responses" => {
            openai_codex_responses::stream_with_client(model, context, options, client)
        }
        "azure-openai-responses" => {
            azure_openai_responses::stream_with_client(model, context, options, client)
        }
        "google-generative-ai" => {
            google_generative_ai::stream_with_client(model, context, options, client)
        }
        "google-vertex" => google_vertex::stream_with_client(model, context, options, client),
        "mistral-conversations" => {
            mistral_conversations::stream_with_client(model, context, options, client)
        }
        "bedrock-converse-stream" => {
            bedrock_converse_stream::stream_with_client(model, context, options, client)
        }
        "pi-messages" => pi_messages::stream_with_client(model, context, options, client),
        _ => unsupported_stream(model, format!("unknown API {api:?}")),
    }
}

fn unsupported_stream(model: Model, error: String) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let mut message = common::empty_message(&model);
    message.stop_reason = StopReason::Error;
    message.error_message = Some(error);
    stream.push(AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: message,
    });
    stream
}
