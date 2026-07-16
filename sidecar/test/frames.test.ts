/**
 * Headless UI bridge tests: real npm pi-tui components instantiated
 * in-sidecar, line frames shipped over the wire, focused input forwarding,
 * dialog RPC round-trips, terminal-input listeners, autocomplete stacking,
 * theme application, and tool renderer slots.
 */

import { describe, expect, test } from "bun:test";
import { join } from "node:path";

import { fromWire } from "../src/protocol.ts";
import type { FrameParamsDto, JsonObject } from "../src/protocol.ts";
import { FIXTURES_DIR, createTestBridge, makeInitParams, makeStateBlock } from "./harness.ts";
import type { RustSide } from "./harness.ts";

async function flushMicrotasks(): Promise<void> {
  for (let i = 0; i < 16; i++) await Promise.resolve();
}

async function bootWithUi(): Promise<RustSide> {
  const bridge = createTestBridge();
  await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "ui-widgets.ts")] }));
  return bridge;
}

function framesOf(bridge: RustSide, slot: string): FrameParamsDto[] {
  return bridge
    .notificationsOf("ui/frame")
    .map((params) => fromWire<FrameParamsDto>(params))
    .filter((frame) => frame.slot === slot);
}

describe("widget frames", () => {
  test("factory widgets render REAL components into shipped line frames", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "show-widget", args: "" });
    await flushMicrotasks();
    const frames = framesOf(bridge, "widget:counter");
    expect(frames.length).toBe(1);
    const frame = frames[0];
    expect(frame?.placement).toBe("belowEditor");
    expect(frame?.version).toBe(1);
    expect(frame?.focusable).toBe(false);
    // Theme-styled output: ANSI escapes from pi's real Theme.fg survive.
    expect(frame?.lines[0]).toContain("count=0 w=80");
    expect(frame?.lines[0]).toContain("\u001b[");
  });

  test("static string[] widgets ship lines directly", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "show-static-widget", args: "" });
    await flushMicrotasks();
    const frames = framesOf(bridge, "widget:banner");
    expect(frames[0]?.lines).toEqual(["line one", "line two"]);
  });

  test("clearing a widget disposes the component and notifies ui/dispose", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "show-widget", args: "" });
    await bridge.peer.request("command/execute", { name: "clear-widget", args: "" });
    await flushMicrotasks();
    const disposals = bridge.notificationsOf("ui/dispose");
    expect(disposals).toContainEqual({ slot: "widget:counter" });
  });

  test("ui/render returns lines at the requested width and pins that width", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "show-widget", args: "" });
    await flushMicrotasks();
    const lines = fromWire<string[]>(
      await bridge.peer.request("ui/render", { slot: "widget:counter", width: 42 }),
    );
    expect(lines[0]).toContain("count=0 w=42");
    // Focused input mutates the component; the re-render uses width 42.
    bridge.peer.notify("ui/input", { slot: "widget:counter", data: "+" });
    await flushMicrotasks();
    const frames = framesOf(bridge, "widget:counter");
    const last = frames[frames.length - 1];
    expect(last?.lines[0]).toContain("count=1 w=42");
    expect(last?.version).toBeGreaterThan(1);
  });

  test("frame versions are monotonic and re-render bursts coalesce", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "show-widget", args: "" });
    await flushMicrotasks();
    const before = framesOf(bridge, "widget:counter").length;
    // Two rapid inputs (each calls tui.requestRender()) within one turn.
    bridge.host.uiBridge?.input("widget:counter", "+");
    bridge.host.uiBridge?.input("widget:counter", "+");
    await flushMicrotasks();
    const frames = framesOf(bridge, "widget:counter");
    // Coalesced: exactly ONE new frame despite two dirty marks.
    expect(frames.length).toBe(before + 1);
    expect(frames[frames.length - 1]?.lines[0]).toContain("count=2");
    const versions = frames.map((frame) => frame.version);
    expect([...versions].sort((a, b) => a - b)).toEqual(versions);
  });
});

describe("footer and status", () => {
  test("setFooter receives a live FooterDataProvider fed by setStatus", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "show-footer", args: "" });
    await flushMicrotasks();
    expect(bridge.notificationsOf("ui/setStatus")).toContainEqual({ key: "fixture", value: "status-live" });
    const frames = framesOf(bridge, "footer");
    expect(frames[0]?.lines[0]).toContain("footer:status-live");
  });

  test("setStatus after the initial flush re-renders the bridged footer leaf", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "show-footer", args: "" });
    await flushMicrotasks();
    // Initial frame flushed with the boot-time status.
    const initial = framesOf(bridge, "footer");
    expect(initial.length).toBe(1);
    expect(initial[0]?.lines[0]).toContain("footer:status-live");

    // A later status mutation alone (oracle: setExtensionStatus + render
    // request) must dirty the footer leaf and ship a fresh frame.
    await bridge.peer.request("command/execute", { name: "set-status", args: "status-later" });
    await flushMicrotasks();
    const frames = framesOf(bridge, "footer");
    expect(frames.length).toBe(2);
    expect(frames[1]?.lines[0]).toContain("footer:status-later");
    expect(frames[1]?.version).toBe(2);
    expect(bridge.notificationsOf("ui/setStatus")).toContainEqual({ key: "fixture", value: "status-later" });
  });
});

describe("dialogs", () => {
  test("select round-trips through the Rust native dialog", async () => {
    const bridge = createTestBridge();
    const seen: JsonObject[] = [];
    bridge.peer.onRequest("ui/select", (params) => {
      seen.push(fromWire<JsonObject>(params));
      return "beta";
    });
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "ui-widgets.ts")] }));
    await bridge.peer.request("command/execute", { name: "ask-select", args: "timed" });
    expect(seen).toEqual([{ title: "Pick one", options: ["alpha", "beta"], timeout: 1500 }]);
    expect(bridge.notificationsOf("action/appendEntry")).toContainEqual({
      customType: "select-result",
      data: { choice: "beta" },
    });
  });

  test("void UI setters emit their notifications and mirror optimistically", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "notify-things", args: "" });
    expect(bridge.notificationsOf("ui/notify")).toContainEqual({ message: "hello there", level: "warning" });
    expect(bridge.notificationsOf("ui/setWorkingMessage")).toContainEqual({ text: "crunching" });
    expect(bridge.notificationsOf("ui/setWorkingVisible")).toContainEqual({ visible: false });
    expect(bridge.notificationsOf("ui/setWorkingIndicator")).toContainEqual({
      options: { frames: ["|", "/"], intervalMs: 120 },
    });
    expect(bridge.notificationsOf("ui/setTitle")).toContainEqual({ text: "fixture title" });
    expect(bridge.notificationsOf("ui/setEditorText")).toContainEqual({ text: "drafted" });
    expect(bridge.notificationsOf("ui/setToolsExpanded")).toContainEqual({ visible: true });
    // Read-after-write inside the same handler observed the writes.
    expect(bridge.notificationsOf("action/appendEntry")).toContainEqual({
      customType: "editor-text",
      data: { text: "drafted", expanded: true },
    });
  });

  test("custom components stay in-sidecar; done() ships ui/done and resolves", async () => {
    const bridge = createTestBridge();
    const customRequests: JsonObject[] = [];
    bridge.peer.onRequest("ui/custom", (params) => {
      customRequests.push(fromWire<JsonObject>(params));
      // The host keeps the dialog open until dismissed; never resolves here.
      const { promise } = Promise.withResolvers<null>();
      return promise;
    });
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "ui-widgets.ts")] }));
    const commandDone = bridge.peer.request("command/execute", { name: "open-custom", args: "" });
    await flushMicrotasks();
    expect(customRequests.length).toBe(1);
    const slot = fromWire<{ slot: string }>(customRequests[0]).slot;
    expect(slot.startsWith("custom:")).toBe(true);
    const frames = framesOf(bridge, slot);
    expect(frames[0]?.focusable).toBe(true);
    expect(frames[0]?.lines[0]).toContain("custom body");
    // Focused key input reaches the component; Enter resolves via done().
    bridge.peer.notify("ui/input", { slot, data: "\r" });
    await commandDone;
    expect(bridge.notificationsOf("ui/done")).toContainEqual({ slot, result: "confirmed" });
    expect(bridge.notificationsOf("action/appendEntry")).toContainEqual({
      customType: "custom-result",
      data: { result: "confirmed" },
    });
  });
});

describe("terminal input listeners", () => {
  test("consume and rewrite semantics match pi's TUI listeners", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("command/execute", { name: "listen-input", args: "" });
    const consumed = await bridge.peer.request("ui/terminal_input", { data: "\u0010" });
    expect(consumed).toEqual({ consume: true });
    const rewritten = await bridge.peer.request("ui/terminal_input", { data: "x" });
    expect(rewritten).toEqual({ consume: false, data: "y" });
    const untouched = await bridge.peer.request("ui/terminal_input", { data: "z" });
    expect(untouched).toEqual({ consume: false });
  });
});

describe("autocomplete", () => {
  test("stacked extension providers answer ui/autocomplete", async () => {
    const bridge = await bootWithUi();
    // Before any provider is stacked: null.
    const empty = await bridge.peer.request("ui/autocomplete", { text: "fi", cursor: 2 });
    expect(empty).toBeNull();
    await bridge.peer.request("command/execute", { name: "stack-autocomplete", args: "" });
    const result = await bridge.peer.request("ui/autocomplete", { text: "fi", cursor: 2 });
    expect(result).toEqual({ items: [{ value: "fixture-item", label: "Fixture Item" }], prefix: "fi" });
  });
});

describe("theme", () => {
  test("ui.theme is pi's live global theme and styles output", async () => {
    const bridge = createTestBridge();
    bridge.peer.onRequest("ui/getAllThemes", () => [{ name: "dark", path: "/host/dark.json" }, { name: "hostonly" }]);
    await bridge.init(makeInitParams({ configuredPaths: [join(FIXTURES_DIR, "ui-widgets.ts")] }));
    await flushMicrotasks();
    await bridge.peer.request("command/execute", { name: "theme-things", args: "" });
    const infos = bridge
      .notificationsOf("action/appendEntry")
      .map((params) => fromWire<{ customType: string; data: { catalog: string[]; styled: string } }>(params))
      .filter((entry) => entry.customType === "theme-info");
    expect(infos.length).toBe(1);
    const info = infos[0];
    // Host catalog is authoritative once fetched.
    expect(info?.data.catalog).toEqual(["dark", "hostonly"]);
    // The styled sample carries real ANSI from the applied wire theme.
    expect(info?.data.styled).toContain("sample");
    expect(info?.data.styled).toContain("\u001b[");
  });

  test("tool renderer slots render via the extension's renderCall/renderResult", async () => {
    const bridge = await bootWithUi();
    await bridge.peer.request("tool/execute", { toolCallId: "rt-1", name: "render_tool", args: { subject: "docs" } });
    const callLines = fromWire<string[]>(await bridge.peer.request("ui/render", { slot: "tool:rt-1:call", width: 60 }));
    expect(callLines[0]).toContain("calling docs @60");
    const resultLines = fromWire<string[]>(
      await bridge.peer.request("ui/render", { slot: "tool:rt-1:result", width: 60 }),
    );
    expect(resultLines[0]).toContain("brief: docs");
    // Expanded state from the state block changes the rendered result.
    await bridge.peer.request(
      "event/emit",
      { event: { type: "agent_start" }, state: fromWire<JsonObject>(makeStateBlock({ toolsExpanded: true })) },
    );
    const expanded = fromWire<string[]>(
      await bridge.peer.request("ui/render", { slot: "tool:rt-1:result", width: 60 }),
    );
    expect(expanded[0]).toContain("full: docs");
  });
});
