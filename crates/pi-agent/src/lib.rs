//! pi-agent: agent loop + tool contract.
//!
//! Port of packages/agent (core loop and types). Harness/session/UI live in
//! later phases.

pub mod agent_loop;
pub mod cancel;
pub mod tools;
pub mod types;

pub use agent_loop::{
    collecting_sink, run_agent_loop, run_agent_loop_continue, unavailable_stream_fn, AgentLoopError,
};
pub use cancel::CancellationToken;
pub use tools::{
    error_tool_result, prepare_and_validate_arguments, prepare_tool_call_arguments,
    validate_tool_arguments,
};
pub use types::*;
