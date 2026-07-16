/**
 * Sidecar host — binds the RPC peer to pi's extension runner.
 *
 * Event ordering (invariant I3): every event dispatch — blocking request or
 * fire-and-forget notification — runs through one serial queue, so a
 * blocking hook's result is applied before the next event dispatches,
 * matching pi's sequential handler semantics.
 *
 * Handler-error asymmetry (invariant I10): all emit paths report handler
 * errors and continue, EXCEPT `tool_call` — `emitToolCall` is uncaught by
 * design in pi (runner.ts), so a throwing handler propagates as an `err`
 * response and Rust fails the tool call.
 */

import { wrapRegisteredTool } from "@earendil-works/pi-coding-agent";
import type {
  BeforeAgentStartEvent,
  ContextEvent,
  ExtensionEvent,
  ExtensionUIContext,
  InputEvent,
  ProjectTrustEvent,
  ToolCallEvent,
  ToolResultEvent,
  UserBashEvent,
} from "@earendil-works/pi-coding-agent";
import type { Api, AssistantMessageEventStream, Context, Model, SimpleStreamOptions } from "@earendil-works/pi-ai";

import { emitProjectTrustEvent } from "./pi-internal.ts";
import type { MessageEndEvent } from "./pi-internal.ts";

import { applyWireTheme, bootRuntime } from "./runtime.ts";
import type { SidecarRuntime } from "./runtime.ts";
import { PI_COMPAT_VERSION, PROTOCOL_VERSION, fromWire, toWire } from "./protocol.ts";
import type {
  InitParamsDto,
  JsonObject,
  JsonValue,
  SessionSyncDto,
  StateBlockDto,
  StateUpdateDto,
  TerminalInputResultDto,
} from "./protocol.ts";
import type { RpcPeer } from "./rpc.ts";

function asObject(value: JsonValue): JsonObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("params must be a JSON object");
  }
  return value;
}

/**
 * UI bridge seam implemented by the headless UI layer (ui-context.ts /
 * frames.ts). The host delegates UI-family inbound traffic here; without a
 * bridge those requests fail and notifications are ignored (print/json
 * modes). Tool-record hooks feed the tool renderer slots.
 */
export interface UiBridge {
  render(slot: string, width: number): string[];
  input(slot: string, data: string): void;
  dispose(slot: string): void;
  terminalInput(data: string): Promise<TerminalInputResultDto>;
  autocomplete(text: string, cursor: number, commandName?: string): Promise<JsonValue>;
  recordToolCall(toolCallId: string, toolName: string, args: unknown): void;
  recordToolUpdate(toolCallId: string, partial: unknown): void;
  recordToolResult(toolCallId: string, result: unknown, isError: boolean): void;
}

export interface SidecarHost {
  peer: RpcPeer;
  /** Set once lifecycle/init completes. */
  runtime: SidecarRuntime | undefined;
  uiBridge: UiBridge | undefined;
  sendHello(): void;
}

export interface HostOptions {
  peer: RpcPeer;
  bunVersion?: string;
  /** Called after lifecycle/shutdown is acknowledged. */
  onShutdown?: () => void;
  /** UI layer factory, invoked at boot (C4 wires this). May decline (no UI mode). */
  createUi?: (runtime: SidecarRuntime) => { context: ExtensionUIContext; bridge: UiBridge } | undefined;
}

export function attachHost(options: HostOptions): SidecarHost {
  const { peer } = options;
  const host: SidecarHost = {
    peer,
    runtime: undefined,
    uiBridge: undefined,
    sendHello: () => {
      peer.notify("lifecycle/hello", {
        protocol: PROTOCOL_VERSION,
        pi: PI_COMPAT_VERSION,
        bun: options.bunVersion ?? process.versions.bun ?? "unknown",
      });
    },
  };

  // Serial event queue (I3).
  let eventQueue: Promise<unknown> = Promise.resolve();
  const enqueue = <T>(work: () => Promise<T>): Promise<T> => {
    const next = eventQueue.then(work, work);
    eventQueue = next.catch(() => {});
    return next;
  };

  peer.onRequest("lifecycle/init", async (params) => {
    const init = fromWire<InitParamsDto>(asObject(params));
    try {
      applyWireTheme(init.theme);
    } catch (error) {
      peer.notify("error/extension", {
        extensionPath: "<sidecar>",
        event: "theme",
        error: error instanceof Error ? error.message : String(error),
      });
    }
    const createUi = options.createUi;
    const runtime = await bootRuntime({
      init,
      peer,
      uiContext:
        createUi === undefined
          ? undefined
          : (booted) => {
              const ui = createUi(booted);
              host.uiBridge = ui?.bridge;
              return ui?.context;
            },
    });
    host.runtime = runtime;
    peer.notify("lifecycle/initialized", {
      registrations: toWire(runtime.registrations()),
      subscribedEvents: runtime.subscribedEvents(),
      errors: toWire(runtime.loadErrors),
    });
    return {};
  });

  peer.onRequest("lifecycle/load", async (params) => {
    const runtime = requireRuntime(host);
    const paths = asObject(params)["paths"];
    if (!Array.isArray(paths)) throw new Error("lifecycle/load requires paths[]");
    const result = await runtime.loadMore(paths.map((path) => String(path)));
    return toWire(result);
  });

  peer.onRequest("lifecycle/shutdown", async () => {
    // Macrotask: the ok response is sent on the microtask queue after this
    // handler resolves; exiting on a microtask would race and drop it.
    setTimeout(() => options.onShutdown?.(), 0);
    return {};
  });

  peer.onNotification("lifecycle/ping", (params) => {
    peer.notify("lifecycle/pong", params);
  });

  peer.onRequest("event/emit", (params, signal) =>
    enqueue(() => dispatchEvent(host, asObject(params), signal)),
  );

  peer.onNotification("event/notify", (params) => {
    void enqueue(() => dispatchEvent(host, asObject(params), undefined)).catch(() => {
      // Fire-and-forget dispatch errors are already reported by the runner's
      // error listener; nothing to return.
    });
  });

  peer.onNotification("session/sync", (params) => {
    requireRuntime(host).session.sync(fromWire<SessionSyncDto>(asObject(params)));
  });

  peer.onNotification("state/update", (params) => {
    const runtime = requireRuntime(host);
    const update = fromWire<StateUpdateDto>(asObject(params));
    // Theme changes re-apply through the mirror's onThemeChange hook.
    runtime.state.applyUpdate(update);
    if (update.idle !== undefined) runtime.bridged.turnSignal.setIdle(update.idle);
  });

  peer.onRequest("tool/execute", async (params, signal) => {
    const runtime = requireRuntime(host);
    const { toolCallId, name, args } = fromWire<{ toolCallId: string; name: string; args: JsonValue }>(
      asObject(params),
    );
    const registered = runtime.runner.getAllRegisteredTools().find((tool) => tool.definition.name === name);
    if (registered === undefined) {
      throw new Error(`extension tool not found: ${name}`);
    }
    host.uiBridge?.recordToolCall(toolCallId, name, args);
    const agentTool = wrapRegisteredTool(registered, runtime.runner);
    let result;
    try {
      result = await agentTool.execute(toolCallId, fromWire(args), signal, (partial) => {
        host.uiBridge?.recordToolUpdate(toolCallId, partial);
        peer.notify("tool/update", { toolCallId, partial: toWire(partial) });
      });
    } catch (error) {
      host.uiBridge?.recordToolResult(
        toolCallId,
        { content: [{ type: "text", text: error instanceof Error ? error.message : String(error) }], details: {} },
        true,
      );
      throw error;
    }
    host.uiBridge?.recordToolResult(toolCallId, result, false);
    // Tool failure is a thrown error -> err frame. Success relays pi's full
    // AgentToolResult surface: addedToolNames (tools introduced from this
    // transcript point) and terminate (stop after the current tool batch)
    // ride the ok payload. The envelope decodes losslessly (result is a
    // Value), but the host's typed ToolExecuteResult must gain the optional
    // fields to consume them — flagged for the Rust-host owner.
    return {
      content: toWire(result.content),
      ...(result.details !== undefined ? { details: toWire(result.details) } : {}),
      isError: false,
      ...(result.addedToolNames !== undefined ? { addedToolNames: result.addedToolNames } : {}),
      ...(result.terminate !== undefined ? { terminate: result.terminate } : {}),
    };
  });

  peer.onRequest("command/execute", async (params) => {
    const runtime = requireRuntime(host);
    const { name, args } = fromWire<{ name: string; args: string }>(asObject(params));
    const command = runtime.runner.getCommand(name);
    if (command === undefined) {
      throw new Error(`extension command not found: ${name}`);
    }
    await command.handler(args, runtime.runner.createCommandContext());
    return {};
  });

  peer.onRequest("shortcut/invoke", async (params) => {
    const runtime = requireRuntime(host);
    const { keyId } = fromWire<{ keyId: string }>(asObject(params));
    const shortcut = runtime.runner.getShortcuts(runtime.keybindingsConfig).get(fromWire(keyId));
    if (shortcut === undefined) {
      throw new Error(`extension shortcut not found: ${keyId}`);
    }
    await shortcut.handler(runtime.runner.createContext());
    return {};
  });

  peer.onRequest("provider/stream", async (params, signal) => {
    const runtime = requireRuntime(host);
    const request = fromWire<{
      streamId: string;
      provider: string;
      model: JsonObject;
      context: JsonObject;
      options?: JsonObject;
    }>(asObject(params));
    const provider = runtime.providers.get(request.provider);
    const streamSimple = provider?.config.streamSimple;
    if (streamSimple === undefined) {
      throw new Error(`provider ${request.provider} has no streamSimple handler`);
    }
    const streamOptions: SimpleStreamOptions = { ...fromWire<SimpleStreamOptions>(request.options ?? {}), signal };
    const stream: AssistantMessageEventStream = streamSimple(
      fromWire<Model<Api>>(request.model),
      fromWire<Context>(request.context),
      streamOptions,
    );
    for await (const event of stream) {
      peer.notify("provider/event", { streamId: request.streamId, event: toWire(event) });
    }
    return toWire(await stream.result());
  });

  // UI family — delegated to the bridge when present (C4).
  peer.onRequest("ui/render", (params) => {
    const { slot, width } = fromWire<{ slot: string; width: number }>(asObject(params));
    return requireUi(host).render(slot, width);
  });
  peer.onNotification("ui/input", (params) => {
    const { slot, data } = fromWire<{ slot: string; data: string }>(asObject(params));
    host.uiBridge?.input(slot, data);
  });
  peer.onNotification("ui/dispose", (params) => {
    const { slot } = fromWire<{ slot: string }>(asObject(params));
    host.uiBridge?.dispose(slot);
  });
  peer.onRequest("ui/terminal_input", async (params) => {
    const { data } = fromWire<{ data: string }>(asObject(params));
    if (host.uiBridge === undefined) return {};
    return toWire(await host.uiBridge.terminalInput(data));
  });
  peer.onRequest("ui/autocomplete", async (params) => {
    const { text, cursor, commandName } = fromWire<{ text: string; cursor: number; commandName?: string }>(
      asObject(params),
    );
    if (host.uiBridge === undefined) return null;
    return toWire(await host.uiBridge.autocomplete(text, cursor, commandName));
  });

  return host;
}

function requireRuntime(host: SidecarHost): SidecarRuntime {
  if (host.runtime === undefined) {
    throw new Error("sidecar runtime is not initialized (lifecycle/init pending)");
  }
  return host.runtime;
}

function requireUi(host: SidecarHost): UiBridge {
  if (host.uiBridge === undefined) {
    throw new Error("no UI bridge is active in this mode");
  }
  return host.uiBridge;
}

/**
 * Route one wire event onto the matching runner emit method and shape the
 * result the way the protocol expects. Runs inside the serial event queue.
 */
async function dispatchEvent(
  host: SidecarHost,
  params: JsonObject,
  signal: AbortSignal | undefined,
): Promise<JsonValue> {
  const runtime = requireRuntime(host);
  const stateBlock = fromWire<StateBlockDto>(params["state"]);
  runtime.state.apply(stateBlock);
  runtime.bridged.turnSignal.setIdle(stateBlock.idle);

  const wireEvent = asObject(params["event"] ?? null);
  const type = wireEvent["type"];
  const runner = runtime.runner;

  switch (type) {
    case "project_trust": {
      const { result, errors } = await emitProjectTrustEvent(
        runtime.loadResult,
        fromWire<ProjectTrustEvent>(wireEvent),
        {
          cwd: String(wireEvent["cwd"] ?? runtime.cwd),
          mode: runtime.mode,
          hasUI: runtime.hasUi,
          ui: runner.getUIContext(),
        },
      );
      for (const error of errors) {
        host.peer.notify("error/extension", toWire(error));
      }
      return toWire(result ?? null);
    }
    case "resources_discover": {
      const discovered = await runner.emitResourcesDiscover(
        String(wireEvent["cwd"] ?? runtime.cwd),
        fromWire<"startup" | "reload">(wireEvent["reason"] ?? "startup"),
      );
      return {
        skillPaths: discovered.skillPaths.map((entry) => entry.path),
        promptPaths: discovered.promptPaths.map((entry) => entry.path),
        themePaths: discovered.themePaths.map((entry) => entry.path),
      };
    }
    case "context": {
      const event = fromWire<ContextEvent>(wireEvent);
      const messages = await runner.emitContext(event.messages);
      return { messages: toWire(messages) };
    }
    case "before_provider_request": {
      const payload = await runner.emitBeforeProviderRequest(wireEvent["payload"]);
      return toWire(payload);
    }
    case "before_provider_headers": {
      const headers = await runner.emitBeforeProviderHeaders(
        fromWire<Record<string, string | null>>(wireEvent["headers"] ?? {}),
      );
      return toWire(headers);
    }
    case "before_agent_start": {
      const event = fromWire<BeforeAgentStartEvent>(wireEvent);
      const combined = await runner.emitBeforeAgentStart(
        event.prompt,
        event.images,
        event.systemPrompt,
        event.systemPromptOptions,
      );
      if (combined === undefined) return null;
      // pi combines every handler's single `message` into `messages`; the
      // wire result's `message` field carries the combined array as a Value.
      return {
        ...(combined.messages !== undefined ? { message: toWire(combined.messages) } : {}),
        ...(combined.systemPrompt !== undefined ? { systemPrompt: combined.systemPrompt } : {}),
      };
    }
    case "message_end": {
      const message = await runner.emitMessageEnd(fromWire<MessageEndEvent>(wireEvent));
      return message === undefined ? null : { message: toWire(message) };
    }
    case "tool_call": {
      // I10: intentionally NOT caught. A throwing handler becomes an err
      // response and Rust fails the tool call (never an error banner).
      const result = await runner.emitToolCall(fromWire<ToolCallEvent>(wireEvent));
      return toWire(result ?? null);
    }
    case "tool_result": {
      const result = await runner.emitToolResult(fromWire<ToolResultEvent>(wireEvent));
      return toWire(result ?? null);
    }
    case "user_bash": {
      const result = await runner.emitUserBash(fromWire<UserBashEvent>(wireEvent));
      return toWire(result ?? null);
    }
    case "input": {
      const event = fromWire<InputEvent>(wireEvent);
      const result = await runner.emitInput(event.text, event.images, event.source, event.streamingBehavior);
      return toWire(result);
    }
    default: {
      const event = withMintedSignal(wireEvent, signal);
      if (type === "session_compact") {
        runtime.bridged.notifySessionCompact(asObject(wireEvent["compactionEntry"] ?? {}));
      }
      // Generic runner path (session_*, agent_*, turn_*, message_*,
      // tool_execution_*, model_select, thinking_level_select, ...).
      const result = await runner.emit(fromWire<never>(event));
      return toWire(result ?? null);
    }
  }
}

/** Events carrying `signal: AbortSignal` in pi get it minted from the RPC request. */
function withMintedSignal(wireEvent: JsonObject, signal: AbortSignal | undefined): ExtensionEvent {
  const type = wireEvent["type"];
  if (type === "session_before_compact" || type === "session_before_tree") {
    return fromWire<ExtensionEvent>({ ...wireEvent, signal: (signal ?? new AbortController().signal) as never });
  }
  return fromWire<ExtensionEvent>(wireEvent);
}
