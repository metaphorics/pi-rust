//! `--mode json` — single-shot JSONL event-stream output.
//!
//! Oracle dispatch: `main.ts` routes `--mode json` through `runPrintMode`
//! with `mode: "json"` (`toPrintOutputMode`). The stdout contract
//! (print-mode.ts:118-121): first line is the `SessionHeader` when the
//! session has one, then every `AgentSessionEvent` as one JSON line via the
//! raw stdout writer; nothing else ever writes stdout.

use std::sync::Arc;

use crate::session::runtime::AgentSessionRuntime;
use crate::wire_out::WireOut;

use super::print::{PrintModeOptions, PrintOutputMode, run_print_mode, run_print_mode_with_out};

/// Run in json mode (print mode with the JSONL event stream on stdout).
pub async fn run_json_mode(
    runtime: Arc<AgentSessionRuntime>,
    mut options: PrintModeOptions,
) -> i32 {
    options.mode = PrintOutputMode::Json;
    run_print_mode(runtime, options).await
}

/// Json-mode core with injectable stdout sink (tests).
pub async fn run_json_mode_with_out(
    runtime: Arc<AgentSessionRuntime>,
    mut options: PrintModeOptions,
    out: Arc<WireOut>,
    register_signals: bool,
) -> i32 {
    options.mode = PrintOutputMode::Json;
    run_print_mode_with_out(runtime, options, out, register_signals).await
}
