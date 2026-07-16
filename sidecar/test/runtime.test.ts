/**
 * Runtime/host integration tests: pi's REAL loader + runner booted in
 * process against fixture extensions, driven through the wire protocol by a
 * fake Rust peer. Covers load lifecycle, registrations, jiti module
 * identity (with-deps), handler ordering and error asymmetry, tool
 * execution with streaming updates, event results, actions, and heartbeat.
 */

import { describe, expect, test } from "bun:test";
import { join } from "node:path";

import { decodeFrame, fromWire } from "../src/protocol.ts";
import type { JsonObject, JsonValue, StateBlockDto } from "../src/protocol.ts";
import { RpcError } from "../src/rpc.ts";
import { FIXTURES_DIR, createTestBridge, makeInitParams, makeStateBlock } from "./harness.ts";
import type { RustSide } from "./harness.ts";

interface OrderGlobal {
  __piFixtureOrder?: string[];
}

function emitParams(event: JsonObject, state?: StateBlockDto): JsonObject {
  return { event, state: fromWire<JsonObject>(state ?? makeStateBlock()) };
}

describe("sidecar host lifecycle", () => {
  test("hello, init, initialized with full registrations", async () => {
    const bridge = createTestBridge();
    const initialized = await bridge.init(
      makeInitParams({
        configuredPaths: [join(FIXTURES_DIR, "simple.ts"), join(FIXTURES_DIR, "with-deps")],
        flagValues: { "simple-verbose": true },
      }),
    );

    const hello = bridge.notificationsOf("lifecycle/hello");
    expect(hello.length).toBe(1);
    expect(fromWire<{ protocol: number; pi: string }>(hello[0]).protocol).toBe(1);
    expect(fromWire<{ pi: string }>(hello[0]).pi).toBe("0.80.7");

    expect(initialized.errors).toEqual([]);
    const toolNames = initialized.registrations.tools.map((tool) => tool.name).sort();
    expect(toolNames).toEqual(["echo_tool", "identity_probe"]);
    const echo = initialized.registrations.tools.find((tool) => tool.name === "echo_tool");
    expect(echo?.label).toBe("Echo");
    expect(echo?.description).toBe("Echo the input text back");
    expect(echo?.hasRenderCall).toBe(false);
    expect(initialized.registrations.commands.map((command) => command.name)).toEqual(["simple-cmd"]);
    expect(initialized.registrations.shortcuts.map((shortcut) => shortcut.keyId)).toEqual(["ctrl+alt+p"]);
    expect(initialized.registrations.flags.map((flag) => flag.name)).toEqual(["simple-verbose"]);
    expect(initialized.subscribedEvents).toEqual(
      ["agent_start", "input", "session_before_compact", "tool_call"].sort(),
    );
  });

  test("ping answers pong with the same nonce", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams());
    bridge.peer.notify("lifecycle/ping", { nonce: 41 });
    const pong = await bridge.waitForNotification("lifecycle/pong");
    expect(pong).toEqual({ nonce: 41 });
  });

  test("load errors are reported per extension without failing init", async () => {
    const bridge = createTestBridge();
    const initialized = await bridge.init(
      makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "does-not-exist.ts")] }),
    );
    expect(initialized.errors.length).toBe(1);
    expect(initialized.errors[0]?.extensionPath).toContain("does-not-exist");
    expect(initialized.errors[0]?.event).toBe("load");
  });
});

describe("jiti module identity (with-deps)", () => {
  test("extension-local deps and the pinned pi package resolve correctly", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "with-deps")] }));
    const result = await bridge.peer.request("tool/execute", {
      toolCallId: "call-identity",
      name: "identity_probe",
      args: { text: "hello" },
    });
    const typed = fromWire<{ content: Array<{ type: string; text: string }>; details: { sameClass: boolean }; isError: boolean }>(result);
    // "dep:" proves the committed extension-local node_modules resolved;
    // ":true" proves the extension's SessionManager class IS the host's
    // (single pinned copy — instanceof works across the jiti boundary).
    expect(typed.content[0]?.text).toBe("dep:hello:true");
    expect(typed.details.sameClass).toBe(true);
    expect(typed.isError).toBe(false);
  });
});

describe("tool execution", () => {
  test("streams partials as tool/update and returns the final result", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    const result = await bridge.peer.request("tool/execute", {
      toolCallId: "call-1",
      name: "echo_tool",
      args: { text: "ping" },
    });
    const updates = bridge.notificationsOf("tool/update");
    expect(updates.length).toBe(1);
    const update = fromWire<{ toolCallId: string; partial: { details: { stage: string } } }>(updates[0]);
    expect(update.toolCallId).toBe("call-1");
    expect(update.partial.details.stage).toBe("half");
    const typed = fromWire<{ content: Array<{ text: string }>; details: { length: number } }>(result);
    expect(typed.content[0]?.text).toBe("echo: ping");
    expect(typed.details.length).toBe(4);
  });

  test("unknown tools fail as err responses", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    expect(
      bridge.peer.request("tool/execute", { toolCallId: "c", name: "no_such_tool", args: {} }),
    ).rejects.toThrow(RpcError);
  });
  test("corpus structured-output relays terminate:true on the ok payload", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "structured-output.ts")] }));
    const result = await bridge.peer.request("tool/execute", {
      toolCallId: "call-so",
      name: "structured_output",
      args: { headline: "H", summary: "S", actionItems: ["a", "b"] },
    });
    const typed = fromWire<{
      content: Array<{ text: string }>;
      details: { headline: string };
      isError: boolean;
      terminate?: boolean;
      addedToolNames?: string[];
    }>(result);
    expect(typed.content[0]?.text).toBe("Saved structured output: H");
    expect(typed.details.headline).toBe("H");
    expect(typed.isError).toBe(false);
    // The batch-stop hint MUST reach the host wire payload.
    expect(typed.terminate).toBe(true);
    expect(typed.addedToolNames).toBeUndefined();
  });

  test("tools activated during execution relay wrapper-computed addedToolNames", async () => {
    const bridge = createTestBridge();
    await bridge.init(
      makeInitParams({
        configuredPaths: [join(FIXTURES_DIR, "dynamic-tools.ts")],
        state: makeStateBlock({ activeTools: ["read", "load_more_tools"] }),
      }),
    );
    const result = await bridge.peer.request("tool/execute", {
      toolCallId: "call-dyn",
      name: "load_more_tools",
      args: {},
    });
    const typed = fromWire<{
      content: Array<{ text: string }>;
      isError: boolean;
      addedToolNames?: string[];
      terminate?: boolean;
    }>(result);
    expect(typed.content[0]?.text).toBe("loaded");
    // pi's wrapRegisteredTool diffs active tools around execute; the
    // bridged mirror must preserve pre-existing names so only the newly
    // activated tool is reported.
    expect(typed.addedToolNames).toEqual(["after_load"]);
    expect(typed.terminate).toBeUndefined();
    const setActive = bridge.notificationsOf("action/setActiveTools");
    expect(setActive).toContainEqual({ toolNames: ["read", "load_more_tools", "after_load"] });
  });
});

describe("event dispatch", () => {
  test("input transform and handled results round-trip", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    const transformed = await bridge.peer.request(
      "event/emit",
      emitParams({ type: "input", text: "rewrite:abc", source: "interactive" }),
    );
    expect(transformed).toEqual({ action: "transform", text: "rewritten:abc" });
    const handled = await bridge.peer.request(
      "event/emit",
      emitParams({ type: "input", text: "swallow", source: "interactive" }),
    );
    expect(handled).toEqual({ action: "handled" });
    const passthrough = await bridge.peer.request(
      "event/emit",
      emitParams({ type: "input", text: "normal", source: "interactive" }),
    );
    expect(passthrough).toEqual({ action: "continue" });
  });

  test("tool_call blocking result round-trips", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    const blocked = await bridge.peer.request(
      "event/emit",
      emitParams({ type: "tool_call", toolCallId: "t1", toolName: "forbidden_tool", input: {} }),
    );
    expect(blocked).toEqual({ block: true, reason: "fixture forbids this tool" });
    const allowed = await bridge.peer.request(
      "event/emit",
      emitParams({ type: "tool_call", toolCallId: "t2", toolName: "fine_tool", input: {} }),
    );
    expect(allowed).toBeNull();
  });

  test("session_before_compact result round-trips (with minted signal)", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    const result = await bridge.peer.request(
      "event/emit",
      emitParams({
        type: "session_before_compact",
        preparation: {},
        branchEntries: [],
        reason: "manual",
        willRetry: false,
      }),
    );
    expect(result).toEqual({ cancel: true });
  });

  test("handlers run strictly in order across extensions and queued events", async () => {
    const globals = globalThis as OrderGlobal;
    globals.__piFixtureOrder = [];
    const bridge = createTestBridge();
    await bridge.init(
      makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "order-a.ts"), join(FIXTURES_DIR, "order-b.ts")] }),
    );
    // A blocking emit and a fire-and-forget notify queued behind it (I3).
    const emitPromise = bridge.peer.request(
      "event/emit",
      emitParams({
        type: "before_agent_start",
        prompt: "p",
        systemPrompt: "s",
        systemPromptOptions: { cwd: "/tmp" },
      }),
    );
    bridge.peer.notify("event/notify", emitParams({ type: "agent_start" }));
    await emitPromise;
    // agent_start handlers run only after the blocking emit fully settled.
    // Within each event, extension order (a before b) is preserved, and a's
    // async handler completes before b starts.
    for (let i = 0; i < 16; i++) await Promise.resolve();
    expect(globals.__piFixtureOrder).toEqual([
      "a:before_agent_start:start",
      "a:before_agent_start:end",
      "b:before_agent_start",
      "a:agent_start",
      "b:agent_start",
    ]);
  });

  test("handler errors: caught paths report error/extension, tool_call propagates", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "throwing.ts")] }));

    // before_agent_start: caught per handler, reported, dispatch succeeds.
    const result = await bridge.peer.request(
      "event/emit",
      emitParams({
        type: "before_agent_start",
        prompt: "p",
        systemPrompt: "s",
        systemPromptOptions: { cwd: "/tmp" },
      }),
    );
    expect(result).toBeNull();
    const errors = bridge.notificationsOf("error/extension");
    expect(errors.length).toBe(1);
    const reported = fromWire<{ extensionPath: string; event: string; error: string }>(errors[0]);
    expect(reported.event).toBe("before_agent_start");
    expect(reported.error).toContain("fixture before_agent_start failure");

    // tool_call: uncaught by design (I10) — the request itself fails.
    try {
      await bridge.peer.request(
        "event/emit",
        emitParams({ type: "tool_call", toolCallId: "t", toolName: "exploding_gate", input: {} }),
      );
      throw new Error("tool_call dispatch should have failed");
    } catch (error) {
      expect(error).toBeInstanceOf(RpcError);
      expect((error as RpcError).detail.message).toContain("fixture tool_call failure");
    }
    // No extra error/extension banner for the tool_call throw.
    expect(bridge.notificationsOf("error/extension").length).toBe(1);
  });
});

describe("compact callback correlation", () => {
  interface CompactCallbackEntry {
    customType: string;
    data: {
      tag: string;
      summary?: string;
      firstKeptEntryId?: string;
      tokensBefore?: number;
      estimatedTokensAfter?: number | null;
      details?: { tag: string } | null;
      message?: string;
    };
  }

  function sessionCompactEvent(tag: string, reason = "manual"): JsonObject {
    return {
      type: "session_compact",
      compactionEntry: {
        type: "compaction",
        id: `c-${tag}`,
        summary: `summary-${tag}`,
        firstKeptEntryId: `keep-${tag}`,
        tokensBefore: 1000,
        estimatedTokensAfter: 250,
        details: { tag },
      },
      fromExtension: false,
      reason,
      willRetry: false,
    };
  }

  function beforeCompactEvent(customInstructions: string): JsonObject {
    return {
      type: "session_before_compact",
      preparation: {},
      branchEntries: [],
      customInstructions,
      reason: "manual",
      willRetry: false,
    };
  }

  async function bootCompactBridge() {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "compact-callbacks.ts")] }));
    return bridge;
  }

  function callbackEntries(bridge: RustSide, customType: string): CompactCallbackEntry["data"][] {
    return bridge
      .notificationsOf("action/appendEntry")
      .map((entry) => fromWire<CompactCallbackEntry>(entry))
      .filter((entry) => entry.customType === customType)
      .map((entry) => entry.data);
  }

  test("each manual session_compact resolves exactly one pending compact in call order", async () => {
    const bridge = await bootCompactBridge();
    await bridge.peer.request("command/execute", { name: "trigger-compact", args: "one" });
    await bridge.peer.request("command/execute", { name: "trigger-compact", args: "two" });
    expect(bridge.notificationsOf("action/compact")).toEqual([
      { options: { customInstructions: "one" } },
      { options: { customInstructions: "two" } },
    ]);

    // One success event settles ONE pending — not all of them.
    await bridge.peer.request("event/emit", emitParams(sessionCompactEvent("a")));
    let completed = callbackEntries(bridge, "compact-complete");
    expect(completed).toEqual([
      {
        tag: "one",
        summary: "summary-a",
        firstKeptEntryId: "keep-a",
        tokensBefore: 1000,
        estimatedTokensAfter: 250,
        details: { tag: "a" },
      },
    ]);

    await bridge.peer.request("event/emit", emitParams(sessionCompactEvent("b")));
    completed = callbackEntries(bridge, "compact-complete");
    expect(completed.length).toBe(2);
    expect(completed[1]).toEqual({
      tag: "two",
      summary: "summary-b",
      firstKeptEntryId: "keep-b",
      tokensBefore: 1000,
      estimatedTokensAfter: 250,
      details: { tag: "b" },
    });
    expect(callbackEntries(bridge, "compact-error")).toEqual([]);
  });

  test("auto compactions (threshold/overflow) never consume pending compacts", async () => {
    const bridge = await bootCompactBridge();
    await bridge.peer.request("command/execute", { name: "trigger-compact", args: "one" });
    await bridge.peer.request("event/emit", emitParams(sessionCompactEvent("auto", "threshold")));
    expect(callbackEntries(bridge, "compact-complete")).toEqual([]);
    await bridge.peer.request("event/emit", emitParams(sessionCompactEvent("manual")));
    expect(callbackEntries(bridge, "compact-complete").map((entry) => entry.tag)).toEqual(["one"]);
  });

  test("a cancelled manual compaction rejects exactly one pending with the oracle error", async () => {
    const bridge = await bootCompactBridge();
    await bridge.peer.request("command/execute", { name: "trigger-compact", args: "cancel-me" });
    const result = await bridge.peer.request("event/emit", emitParams(beforeCompactEvent("cancel-me")));
    expect(result).toEqual({ cancel: true });
    expect(callbackEntries(bridge, "compact-error")).toEqual([
      { tag: "cancel-me", message: "Compaction cancelled" },
    ]);
    expect(callbackEntries(bridge, "compact-complete")).toEqual([]);
  });

  test("concurrent success and failure settle their own callbacks", async () => {
    const bridge = await bootCompactBridge();
    await bridge.peer.request("command/execute", { name: "trigger-compact", args: "first-ok" });
    await bridge.peer.request("command/execute", { name: "trigger-compact", args: "cancel-me" });

    // Host runs the first compaction: not cancelled, then completes.
    const first = await bridge.peer.request("event/emit", emitParams(beforeCompactEvent("first-ok")));
    expect(first).toBeNull();
    await bridge.peer.request("event/emit", emitParams(sessionCompactEvent("a")));
    // Second compaction: the before hook cancels it.
    const second = await bridge.peer.request("event/emit", emitParams(beforeCompactEvent("cancel-me")));
    expect(second).toEqual({ cancel: true });

    expect(callbackEntries(bridge, "compact-complete").map((entry) => entry.tag)).toEqual(["first-ok"]);
    expect(callbackEntries(bridge, "compact-error")).toEqual([
      { tag: "cancel-me", message: "Compaction cancelled" },
    ]);
  });
});

describe("actions and state", () => {
  test("command execution appends entries optimistically and notifies Rust", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    await bridge.peer.request("command/execute", { name: "simple-cmd", args: "--fast" });
    const appended = bridge.notificationsOf("action/appendEntry");
    expect(appended).toEqual([{ customType: "simple-cmd-ran", data: { args: "--fast" } }]);
    // Read-after-write: the optimistic entry is in the mirror.
    const entries = bridge.host.runtime?.session.sessionManager.getEntries() ?? [];
    expect(entries.some((entry) => entry.type === "custom")).toBe(true);
  });

  test("shortcut invocation reaches the extension handler", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    await bridge.peer.request("shortcut/invoke", { keyId: "ctrl+alt+p" });
    expect(bridge.notificationsOf("action/appendEntry")).toEqual([
      { customType: "simple-shortcut-ran", data: {} },
    ]);
  });

  test("state blocks piggybacked on events refresh sync getters", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    await bridge.peer.request(
      "event/emit",
      emitParams({ type: "agent_start" }, makeStateBlock({ thinkingLevel: "xhigh", idle: false })),
    );
    const runtime = bridge.host.runtime;
    expect(runtime?.state.current.thinkingLevel).toBe("xhigh");
    expect(runtime?.bridged.turnSignal.signal).toBeDefined();
    // state/update flips idle back; the turn signal retires unaborted.
    bridge.peer.notify("state/update", { idle: true });
    expect(runtime?.bridged.turnSignal.signal).toBeUndefined();
  });

  test("session/sync notifications hydrate the mirror", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [] }));
    bridge.peer.notify("session/sync", {
      epoch: 1,
      sessionFile: "/sessions/x.jsonl",
      header: { type: "session", version: 3, id: "s1", timestamp: "t", cwd: "/tmp" },
      entries: [
        { type: "message", id: "m1", parentId: null, timestamp: "t", message: { role: "user", content: "hi" } },
      ],
      leafId: "m1",
    });
    expect(bridge.host.runtime?.session.sessionManager.getLeafId()).toBe("m1");
  });
});

describe("protocol purity", () => {
  test("every sidecar byte is a decodable NDJSON frame", async () => {
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "simple.ts")] }));
    await bridge.peer.request("event/emit", emitParams({ type: "agent_start" }));
    for (const frame of bridge.rawFrames) {
      expect(() => decodeFrame(frame)).not.toThrow();
    }
    expect(bridge.transportErrors).toEqual([]);
  });
});

describe("lifecycle/load (in-place extension addition)", () => {
  test("adds new extensions without rerunning loaded factories", async () => {
    const globals = globalThis as OrderGlobal;
    globals.__piFixtureOrder = [];
    const bridge = createTestBridge();
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "order-a.ts")] }));
    const result = await bridge.peer.request("lifecycle/load", {
      paths: [join(FIXTURES_DIR, "order-a.ts"), join(FIXTURES_DIR, "simple.ts")],
    });
    const typed = fromWire<{ registrations: { tools: Array<{ name: string }> }; errors: unknown[] }>(result);
    expect(typed.errors).toEqual([]);
    expect(typed.registrations.tools.map((tool) => tool.name)).toEqual(["echo_tool"]);
    // order-a's factory did not re-run and both extensions serve events.
    await bridge.peer.request("event/emit", emitParams({ type: "agent_start" }));
    for (let i = 0; i < 16; i++) await Promise.resolve();
    expect(globals.__piFixtureOrder).toEqual(["a:agent_start"]);
    const swallow = await bridge.peer.request(
      "event/emit",
      emitParams({ type: "input", text: "swallow", source: "interactive" }),
    );
    expect(swallow).toEqual({ action: "handled" });
  });
});
