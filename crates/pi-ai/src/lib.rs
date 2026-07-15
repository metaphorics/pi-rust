//! pi-ai: types, event stream, 10 wire protocols, auth, model catalog.
//!
//! Port of packages/ai. Skeleton only for Phase 0.

pub mod event_stream;
pub mod types;

pub mod api;
pub mod http;
pub mod json_parse;
pub mod sse;

pub use event_stream::{AssistantMessageEventStream, create_assistant_message_event_stream};
pub use types::*;
