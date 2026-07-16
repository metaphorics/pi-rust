# Task Report: P6 Tool Result Protocol Extension

## Objective
Extend `ToolExecuteResult` to support the optional `terminate` (`bool`) and `addedToolNames` (`string[]`) fields, ensuring exact camelCase/omission/order parity between Rust and the sidecar, while keeping the old JSON format compatible and preserving new fields.

## Protocol Versioning Decision
- Both Rust and sidecar protocol implementations remain at version `1` (`PROTOCOL_VERSION = 1`).
- This change is fully backward-compatible and additive: old payloads (without `addedToolNames` and `terminate`) continue to decode correctly, and when the optional fields are absent (`None`), they are omitted from the encoded JSON frames.
- No version bump is required as there is no breaking change to the protocol.

## Changes

### 1. Rust Protocol DTO (`crates/pi-ext-protocol`)
- Extended `ToolExecuteResult` in `crates/pi-ext-protocol/src/lib.rs` with:
  ```rust
  #[serde(skip_serializing_if = "Option::is_none")]
  pub added_tool_names: Option<Vec<String>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub terminate: Option<bool>,
  ```
  in the exact camelCase mapping and field ordering to match `ToolExecuteResultDto` on the sidecar.
- Added comprehensive unit tests inside `crates/pi-ext-protocol/src/lib.rs` (`typed_success_payload_decodes_without_shape_loss`) to:
  - Verify that old payloads (lacking `addedToolNames`/`terminate`) decode successfully.
  - Verify that the optional fields are omitted from serialized output when `None` (omission serialization check).
  - Verify that the new payloads (with `addedToolNames` and `terminate`) decode successfully and those fields survive.
  - Verify response ok-decoding round-trip.

### 2. Golden Fixtures & Tests
- Created `crates/pi-ext-protocol/fixtures/sidecar-to-rust-tool-execute-result.json` representing a response to `tool/execute` with the new fields:
  ```json
  {"type":"res","id":6,"ok":{"content":[{"type":"text","text":"saved"}],"details":{"progress":1},"isError":false,"addedToolNames":["new_tool"],"terminate":true}}
  ```
- Registered and verified this new fixture in:
  - `crates/pi-ext-protocol/tests/golden.rs` (`RESULT_FIXTURES` and `result_payloads_are_byte_exact_and_typed`)
  - `sidecar/test/golden.test.ts` (`RESULT_FIXTURES`, length check updated to `21`, and envelope reconstruction tests)

### 3. Sidecar Compile Mirror Test (`sidecar`)
- Created `sidecar/test/protocol-typecheck.test.ts` containing compile-time/typecheck assertions:
  - Asserts that `Pick<AgentToolResult<unknown>, "addedToolNames" | "terminate">` matches `Pick<ToolExecuteResultDto, "addedToolNames" | "terminate">` bidirectionally.
  - Asserts that a literal with both new fields satisfies `ToolExecuteResultDto`.
  - Runs in typecheck phase (`bun run typecheck`) and the bun test phase (`bun test`).

## Verification Results
- **Workspace Cargo Tests**: `cargo test --workspace` passes cleanly (621 tests, 51 suites).
- **Workspace Cargo Clippy**: `cargo clippy --workspace -- -D warnings` is clean.
- **Sidecar Bun Tests**: `bun test` passes cleanly (95 tests, 6 files).
- **Sidecar Typecheck**: `bun run typecheck` (`tsc --noEmit`) passes cleanly with zero errors.
