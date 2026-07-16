//! Non-interactive run modes (`print`, `json`, `rpc`).
//!
//! Port of `packages/coding-agent/src/modes/` minus interactive (wave C).
//! Invariant (global constraint / output-guard.ts): in these modes NOTHING
//! writes stdout except [`crate::wire_out::WireOut`]; diagnostics go to
//! stderr.

pub mod rpc;
