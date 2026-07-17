/**
 * TypeScript mirror of `crates/pi-ext-protocol` (the single source of truth).
 *
 * Wire format: one JSON object per line (NDJSON) over the sidecar's stdio.
 * Every shape here is golden-fixture-locked against the Rust crate's fixtures
 * (`crates/pi-ext-protocol/fixtures/*.json`) — see `sidecar/test/golden.test.ts`.
 */

export const PROTOCOL_VERSION = 1;
export const PI_COMPAT_VERSION = "0.80.7";
export const MAX_FRAME_BYTES = 8 * 1024 * 1024;
export const MAX_ERROR_MESSAGE_BYTES = 64 * 1024;
export const MAX_ERROR_STACK_BYTES = 512 * 1024;

/** JSON value as it appears on the wire. */
export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
export type JsonObject = { [key: string]: JsonValue };



/**
 * Wire boundary seam. Values crossing between wire JSON and pi's typed
 * surfaces are structurally identical (fixture-locked protocol mirror) but
 * nominally unrelated to TS; these are the single documented conversion
 * points in each direction. JSON validity of `toWire` inputs is guaranteed
 * by construction: pi serializes the same objects itself.
 */
export function toWire(value: unknown): JsonValue {
  return value as JsonValue;
}

export function fromWire<T>(value: unknown): T {
  return value as T;
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

export interface RequestFrame {
  type: "req";
  id: number;
  method: RequestMethod;
  params: JsonValue;
}

export interface OkResponseFrame {
  type: "res";
  id: number;
  ok: JsonValue;
}

export interface ErrResponseFrame {
  type: "res";
  id: number;
  err: ProtocolError;
}

export type ResponseFrame = OkResponseFrame | ErrResponseFrame;

export interface EventFrame {
  type: "ev";
  method: NotificationMethod;
  params: JsonValue;
}

export interface CancelFrame {
  type: "cancel";
  id: number;
}

export type Envelope = RequestFrame | ResponseFrame | EventFrame | CancelFrame;

export interface ProtocolError {
  code: string;
  message: string;
  stack?: string;
  extensionPath?: string;
}

/** Request methods — mirrors `pi_ext_protocol::Request`. */
export const REQUEST_METHODS = [
  "lifecycle/init",
  "lifecycle/load",
  "lifecycle/shutdown",
  "event/emit",
  "action/setModel",
  "action/waitForIdle",
  "action/newSession",
  "action/fork",
  "action/navigateTree",
  "action/switchSession",
  "action/reload",
  "action/replaced/sendMessage",
  "action/replaced/sendUserMessage",
  "ui/select",
  "ui/confirm",
  "ui/input",
  "ui/editor",
  "ui/custom",
  "ui/render",
  "ui/autocomplete",
  "ui/terminal_input",
  "ui/getAllThemes",
  "ui/getTheme",
  "tool/execute",
  "provider/stream",
  "command/execute",
  "shortcut/invoke",
  "session/setup",
] as const;
export type RequestMethod = (typeof REQUEST_METHODS)[number];

/** Notification methods — mirrors `pi_ext_protocol::Notification`. */
export const NOTIFICATION_METHODS = [
  "lifecycle/hello",
  "lifecycle/initialized",
  "lifecycle/ping",
  "lifecycle/pong",
  "event/notify",
  "action/sendMessage",
  "action/sendUserMessage",
  "action/appendEntry",
  "action/setSessionName",
  "action/setLabel",
  "action/setActiveTools",
  "action/refreshTools",
  "action/setThinkingLevel",
  "action/shutdown",
  "action/abort",
  "action/compact",
  "ui/notify",
  "ui/setStatus",
  "ui/setWorkingMessage",
  "ui/setWorkingVisible",
  "ui/setWorkingIndicator",
  "ui/setHiddenThinkingLabel",
  "ui/setTitle",
  "ui/setEditorText",
  "ui/pasteToEditor",
  "ui/setTheme",
  "ui/setToolsExpanded",
  "ui/frame",
  "ui/input",
  "ui/dispose",
  "ui/done",
  "ui/overlay",
  "ui/focus",
  "ui/resize",
  "ui/editorSubmit",
  "ui/editorChange",
  "ui/terminalInputActive",
  "tool/update",
  "provider/register",
  "provider/unregister",
  "provider/event",
  "session/sync",
  "state/update",
  "error/extension",
] as const;

export interface ThemeCatalogEntry {
  name: string;
  path?: string;
}
export type NotificationMethod = (typeof NOTIFICATION_METHODS)[number];

const REQUEST_METHOD_SET: ReadonlySet<string> = new Set(REQUEST_METHODS);
const NOTIFICATION_METHOD_SET: ReadonlySet<string> = new Set(NOTIFICATION_METHODS);

// ---------------------------------------------------------------------------
// Method param DTOs (structural mirrors; wire schema owned by the Rust crate)
// ---------------------------------------------------------------------------

export type ExtensionModeDto = "tui" | "rpc" | "json" | "print";
export type FlagValueDto = boolean | string;

export interface ThemeDto {
  name: string;
  json: JsonValue;
}

export interface SessionSnapshotDto {
  epoch: number;
  sessionFile: string;
  header?: JsonValue;
  entries: JsonValue[];
  leafId: string | null;
  name?: string;
}

export interface SessionSyncDto {
  epoch: number;
  sessionFile: string;
  header?: JsonValue;
  entries?: JsonValue[];
  appended?: JsonValue[];
  leafId: string | null;
  name?: string;
}

export interface ContextUsageDto {
  tokens: number | null;
  contextWindow: number;
  percent: number | null;
}

export interface StateBlockDto {
  sessionName?: string;
  model?: JsonObject;
  idle: boolean;
  projectTrusted: boolean;
  pendingMessages: boolean;
  activeTools: string[];
  allTools: ToolInfoDto[];
  commands: CommandInfoDto[];
  thinkingLevel: string;
  contextUsage?: ContextUsageDto;
  systemPrompt: string;
  systemPromptOptions?: JsonObject;
  flagValues: Record<string, FlagValueDto>;
  editorText: string;
  toolsExpanded: boolean;
  footer?: JsonValue;
  theme: ThemeDto;
}

export interface StateUpdateDto {
  model?: JsonObject;
  idle?: boolean;
  thinkingLevel?: string;
  activeTools?: string[];
  allTools?: ToolInfoDto[];
  contextUsage?: ContextUsageDto;
  systemPrompt?: string;
  footer?: JsonValue;
  theme?: ThemeDto;
  editorText?: string;
  toolsExpanded?: boolean;
}

export interface InitParamsDto {
  cwd: string;
  agentDir: string;
  sessionDir: string;
  configuredPaths: string[];
  mode: ExtensionModeDto;
  hasUi: boolean;
  flagValues: Record<string, FlagValueDto>;
  theme: ThemeDto;
  terminalSize?: { width: number; height: number };
  session: SessionSnapshotDto;
  state: StateBlockDto;
}

export interface ToolInfoDto {
  name: string;
  description: string;
  parameters: JsonValue;
  promptGuidelines?: string;
  sourceInfo: SourceInfoDto;
}

export interface CommandInfoDto {
  name: string;
  description?: string;
}

export interface SourceInfoDto {
  path: string;
  source: string;
  scope: "user" | "project" | "temporary";
  origin: "package" | "top-level";
  baseDir?: string;
}

export interface ToolRegistrationDto {
  name: string;
  label: string;
  description: string;
  parameters: JsonValue;
  promptSnippet?: string;
  promptGuidelines?: string;
  sourceInfo: SourceInfoDto;
  hasRenderCall: boolean;
  hasRenderResult: boolean;
}

export interface CommandRegistrationDto {
  name: string;
  description?: string;
  sourceInfo: SourceInfoDto;
  hasArgumentCompletions: boolean;
}

export interface ShortcutRegistrationDto {
  keyId: string;
  description?: string;
  extensionPath: string;
}

export interface FlagRegistrationDto {
  name: string;
  description?: string;
  type: "boolean" | "string";
  default?: FlagValueDto;
  extensionPath: string;
}

export interface ProviderRegistrationDto {
  name: string;
  configDto: JsonValue;
  hasStreamSimple?: boolean;
  extensionPath?: string;
}

export interface RegistrationsDto {
  tools: ToolRegistrationDto[];
  commands: CommandRegistrationDto[];
  shortcuts: ShortcutRegistrationDto[];
  flags: FlagRegistrationDto[];
  providers: ProviderRegistrationDto[];
}

export interface ExtensionErrorDto {
  extensionPath: string;
  event: string;
  error: string;
  stack?: string;
}

export interface InitializedParamsDto {
  registrations: RegistrationsDto;
  subscribedEvents: string[];
  errors: ExtensionErrorDto[];
}

export interface HelloParamsDto {
  protocol: number;
  pi: string;
  bun: string;
}

export interface FrameParamsDto {
  slot: string;
  lines: string[];
  version: number;
  wantsKeyRelease: boolean;
  focusable: boolean;
  placement?: "aboveEditor" | "belowEditor";
}

export interface TerminalInputResultDto {
  consume?: boolean;
  data?: string;
}

export interface ToolExecuteResultDto {
  content: JsonValue[];
  details?: JsonValue;
  isError: boolean;
  /** Names of tools introduced by this result (AgentToolResult.addedToolNames). */
  addedToolNames?: string[];
  /** Early-termination hint: stop after the current tool batch (AgentToolResult.terminate). */
  terminate?: boolean;
}

// ---------------------------------------------------------------------------
// Frame codec
// ---------------------------------------------------------------------------

export type FrameErrorCode = "empty" | "oversize" | "multiple_lines" | "malformed" | "invalid";

export class FrameError extends Error {
  constructor(
    readonly code: FrameErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "FrameError";
  }
}

/**
 * serde_json prints Rust `f64` fields with a trailing `.0` when the value is
 * integral (e.g. `"input":0.0`), where JS `JSON.stringify` prints `0`.
 *
 * To keep encoding byte-identical to the Rust crate, float formatting is
 * driven by the exact typed-f64 paths of the protocol DTOs, keyed by method.
 * Passthrough `Value` payloads (extension args, session entries, custom
 * data) are NEVER reformatted — serde itself round-trips those as parsed.
 *
 * `*` matches one array index.
 */
const MODEL_COST_FLOATS = (prefix: string): string[] => [
  `${prefix}.cost.input`,
  `${prefix}.cost.output`,
  `${prefix}.cost.cacheRead`,
  `${prefix}.cost.cacheWrite`,
  `${prefix}.cost.tiers.*.input`,
  `${prefix}.cost.tiers.*.output`,
  `${prefix}.cost.tiers.*.cacheRead`,
  `${prefix}.cost.tiers.*.cacheWrite`,
];

const STATE_BLOCK_FLOATS = (prefix: string): string[] => [
  ...MODEL_COST_FLOATS(`${prefix}.model`),
  `${prefix}.contextUsage.percent`,
];

const FLOAT_PATHS_BY_METHOD: Record<string, ReadonlySet<string>> = {
  "action/setModel": new Set(MODEL_COST_FLOATS("params.model")),
  "provider/stream": new Set(MODEL_COST_FLOATS("params.model")),
  "lifecycle/init": new Set(STATE_BLOCK_FLOATS("params.state")),
  "event/emit": new Set([
    ...STATE_BLOCK_FLOATS("params.state"),
    ...MODEL_COST_FLOATS("params.event.model"),
    ...MODEL_COST_FLOATS("params.event.previousModel"),
  ]),
  "event/notify": new Set([
    ...STATE_BLOCK_FLOATS("params.state"),
    ...MODEL_COST_FLOATS("params.event.model"),
    ...MODEL_COST_FLOATS("params.event.previousModel"),
  ]),
  "state/update": new Set([...MODEL_COST_FLOATS("params.model"), "params.contextUsage.percent"]),
};

const NO_FLOAT_PATHS: ReadonlySet<string> = new Set();

function serializeValue(value: unknown, path: string, floatPaths: ReadonlySet<string>): string {
  if (value === null) return "null";
  switch (typeof value) {
    case "boolean":
      return value ? "true" : "false";
    case "number": {
      if (!Number.isFinite(value)) {
        throw new FrameError("malformed", `non-finite number is not representable in JSON: ${value}`);
      }
      if (Number.isInteger(value) && floatPaths.has(path)) {
        return `${value}.0`;
      }
      return JSON.stringify(value);
    }
    case "string":
      return JSON.stringify(value);
    case "object":
      break;
    default:
      throw new FrameError("malformed", `value of type ${typeof value} is not representable in JSON`);
  }
  if (Array.isArray(value)) {
    const itemPath = `${path}.*`;
    return `[${value.map((item) => serializeValue(item, itemPath, floatPaths)).join(",")}]`;
  }
  const obj = value as Record<string, unknown>;
  const parts: string[] = [];
  for (const [k, v] of Object.entries(obj)) {
    if (v === undefined) continue;
    parts.push(`${JSON.stringify(k)}:${serializeValue(v, path.length === 0 ? k : `${path}.${k}`, floatPaths)}`);
  }
  return `{${parts.join(",")}}`;
}

/** Serialize an envelope with canonical top-level key order. */
function serializeEnvelope(envelope: Envelope): string {
  const ordered: Record<string, unknown> = { type: envelope.type };
  switch (envelope.type) {
    case "req":
      ordered["id"] = envelope.id;
      ordered["method"] = envelope.method;
      ordered["params"] = envelope.params;
      break;
    case "res":
      ordered["id"] = envelope.id;
      if ("ok" in envelope) {
        ordered["ok"] = envelope.ok;
      } else {
        ordered["err"] = orderProtocolError(envelope.err);
      }
      break;
    case "ev":
      ordered["method"] = envelope.method;
      ordered["params"] = envelope.params;
      break;
    case "cancel":
      ordered["id"] = envelope.id;
      break;
  }
  const method = envelope.type === "req" || envelope.type === "ev" ? envelope.method : undefined;
  const floatPaths = (method !== undefined ? FLOAT_PATHS_BY_METHOD[method] : undefined) ?? NO_FLOAT_PATHS;
  return serializeValue(ordered, "", floatPaths);
}

function orderProtocolError(err: ProtocolError): Record<string, unknown> {
  const ordered: Record<string, unknown> = { code: err.code, message: err.message };
  if (err.stack !== undefined) ordered["stack"] = err.stack;
  if (err.extensionPath !== undefined) ordered["extensionPath"] = err.extensionPath;
  return ordered;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

/** Encode one envelope into an NDJSON frame (validated, newline-terminated). */
export function encodeFrame(envelope: Envelope): Uint8Array {
  validateEnvelope(envelope);
  const body = serializeEnvelope(envelope);
  const bytes = encoder.encode(`${body}\n`);
  if (bytes.length > MAX_FRAME_BYTES) {
    throw new FrameError("oversize", `NDJSON frame is ${bytes.length} bytes, maximum is ${MAX_FRAME_BYTES}`);
  }
  return bytes;
}

/** Decode and validate one NDJSON frame. Mirrors `pi_ext_protocol::decode_frame`. */
export function decodeFrame(frame: Uint8Array): Envelope {
  if (frame.length === 0 || isBlankFrame(frame)) {
    throw new FrameError("empty", "empty NDJSON frame");
  }
  if (frame.length > MAX_FRAME_BYTES) {
    throw new FrameError("oversize", `NDJSON frame is ${frame.length} bytes, maximum is ${MAX_FRAME_BYTES}`);
  }
  let body = frame;
  if (body[body.length - 1] === 0x0a) body = body.subarray(0, body.length - 1);
  if (body[body.length - 1] === 0x0d) body = body.subarray(0, body.length - 1);
  for (const byte of body) {
    if (byte === 0x0a || byte === 0x0d) {
      throw new FrameError("multiple_lines", "frame contains more than one JSON line");
    }
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(decoder.decode(body));
  } catch (error) {
    throw new FrameError("malformed", `malformed JSON frame: ${String(error)}`);
  }
  const envelope = parseEnvelope(parsed);
  validateEnvelope(envelope);
  return envelope;
}

function isBlankFrame(frame: Uint8Array): boolean {
  if (frame.length === 1 && frame[0] === 0x0a) return true;
  return frame.length === 2 && frame[0] === 0x0d && frame[1] === 0x0a;
}

function parseEnvelope(parsed: unknown): Envelope {
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new FrameError("malformed", "frame is not a JSON object");
  }
  const obj = parsed as Record<string, unknown>;
  const type = obj["type"];
  switch (type) {
    case "req": {
      const id = parseRequestId(obj["id"]);
      const method = obj["method"];
      if (typeof method !== "string" || !REQUEST_METHOD_SET.has(method)) {
        throw new FrameError("malformed", `unknown request method: ${String(method)}`);
      }
      if (!("params" in obj)) {
        throw new FrameError("malformed", "request frame is missing params");
      }
      return { type: "req", id, method: method as RequestMethod, params: obj["params"] as JsonValue };
    }
    case "res": {
      const id = parseRequestId(obj["id"]);
      if ("ok" in obj) {
        return { type: "res", id, ok: obj["ok"] as JsonValue };
      }
      const err = obj["err"];
      if (typeof err !== "object" || err === null) {
        throw new FrameError("malformed", "response frame has neither ok nor err");
      }
      const errObj = err as Record<string, unknown>;
      if (typeof errObj["code"] !== "string" || typeof errObj["message"] !== "string") {
        throw new FrameError("malformed", "protocol error requires string code and message");
      }
      const protocolError: ProtocolError = { code: errObj["code"], message: errObj["message"] };
      if (typeof errObj["stack"] === "string") protocolError.stack = errObj["stack"];
      if (typeof errObj["extensionPath"] === "string") protocolError.extensionPath = errObj["extensionPath"];
      return { type: "res", id, err: protocolError };
    }
    case "ev": {
      const method = obj["method"];
      if (typeof method !== "string" || !NOTIFICATION_METHOD_SET.has(method)) {
        throw new FrameError("malformed", `unknown notification method: ${String(method)}`);
      }
      if (!("params" in obj)) {
        throw new FrameError("malformed", "event frame is missing params");
      }
      return { type: "ev", method: method as NotificationMethod, params: obj["params"] as JsonValue };
    }
    case "cancel":
      return { type: "cancel", id: parseRequestId(obj["id"]) };
    default:
      throw new FrameError("malformed", `unknown frame type: ${String(type)}`);
  }
}

function parseRequestId(id: unknown): number {
  if (typeof id !== "number" || !Number.isInteger(id) || id < 1) {
    throw new FrameError("malformed", `request id must be a positive integer, got ${String(id)}`);
  }
  return id;
}

/** Mirrors `pi_ext_protocol::validate_envelope`. */
function validateEnvelope(envelope: Envelope): void {
  if (envelope.type === "res" && "err" in envelope) {
    validateErrorText(envelope.err.message, envelope.err.stack);
    return;
  }
  if (envelope.type !== "ev") return;
  if (envelope.method === "lifecycle/hello") {
    const params = envelope.params as Partial<HelloParamsDto> | null;
    if (params && params.protocol !== PROTOCOL_VERSION) {
      throw new FrameError(
        "invalid",
        `unsupported extension protocol version ${String(params.protocol)}; supported version is ${PROTOCOL_VERSION}`,
      );
    }
  } else if (envelope.method === "error/extension") {
    const params = envelope.params as Partial<ExtensionErrorDto> | null;
    if (params) validateErrorText(params.error ?? "", params.stack);
  } else if (envelope.method === "lifecycle/initialized") {
    const params = envelope.params as Partial<InitializedParamsDto> | null;
    for (const error of params?.errors ?? []) {
      validateErrorText(error.error, error.stack);
    }
  }
}

function validateErrorText(message: string, stack: string | undefined): void {
  if (encoder.encode(message).length > MAX_ERROR_MESSAGE_BYTES) {
    throw new FrameError("invalid", "protocol error message exceeds its size bound");
  }
  if (stack !== undefined && encoder.encode(stack).length > MAX_ERROR_STACK_BYTES) {
    throw new FrameError("invalid", "protocol error stack exceeds its size bound");
  }
}
