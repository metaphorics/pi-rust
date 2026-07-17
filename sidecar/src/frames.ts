/**
 * Frame bridge — the sidecar side of the extension-UI leaf-frame contract
 * (architecture packet §5).
 *
 * Component instances live HERE, next to the extension code that created
 * them; only styled line frames cross the wire. One headless npm pi-tui
 * `TUI` runs on a virtual terminal so factory `tui` params behave exactly
 * like pi's; `tui.requestRender()` maps to marking every live slot dirty.
 *
 * Scheduler: dirty slots coalesce per microtask and ship `ui/frame`
 * latest-wins — never more than one pending frame per slot; `version` is
 * monotonic per slot and Rust drops stale frames.
 */

import { TUI, isFocusable } from "@earendil-works/pi-tui";
import type { Component } from "@earendil-works/pi-tui";
import type { RegisteredTool } from "@earendil-works/pi-coding-agent";
import type { AgentToolResult } from "@earendil-works/pi-coding-agent";

import { activeTheme } from "./pi-internal.ts";
import type { ToolRenderContext } from "./pi-internal.ts";
import { toWire } from "./protocol.ts";
import type { JsonValue, TerminalInputResultDto } from "./protocol.ts";
import type { RpcPeer } from "./rpc.ts";
import { VirtualTerminal } from "./virtual-terminal.ts";

export type DisposableComponent = Component & { dispose?(): void };

export type WidgetPlacementDto = "aboveEditor" | "belowEditor";

interface ComponentSlot {
  kind: "component";
  component: DisposableComponent;
  width: number;
  version: number;
  focusable: boolean;
  placement?: WidgetPlacementDto;
  dirty: boolean;
}

interface StaticSlot {
  kind: "static";
  lines: string[];
  width: number;
  version: number;
  placement?: WidgetPlacementDto;
  dirty: boolean;
}

type Slot = ComponentSlot | StaticSlot;

/** Per-tool-call render bookkeeping (ToolRenderContext contract). */
export interface ToolRenderRecord {
  tool: RegisteredTool;
  args: unknown;
  result?: AgentToolResult<unknown>;
  isPartial: boolean;
  isError: boolean;
  executionStarted: boolean;
  state: unknown;
  lastCallComponent?: Component;
  lastResultComponent?: Component;
}

export interface FrameBridgeDeps {
  peer: RpcPeer;
  cwd: string;
  /** Current tool-output expansion state (from the state mirror). */
  toolsExpanded: () => boolean;
  /** Dynamic slot resolution hooks (session entries / custom messages). */
  resolveEntryComponent?: (entryId: string) => Component | undefined;
  resolveMessageComponent?: (messageKey: string) => Component | undefined;
}

const DEFAULT_WIDTH = 80;

export class FrameBridge {
  readonly terminal = new VirtualTerminal();
  readonly tui: TUI;
  private readonly slots = new Map<string, Slot>();
  private readonly toolRecords = new Map<string, ToolRenderRecord>();
  private flushScheduled = false;
  private readonly deps: FrameBridgeDeps;

  constructor(deps: FrameBridgeDeps) {
    this.deps = deps;
    this.tui = new TUI(this.terminal);
    // Headless: components calling tui.requestRender() must repaint their
    // bridged frames, not a real terminal.
    this.tui.requestRender = () => {
      for (const slot of this.slots.keys()) this.markDirty(slot);
    };
  }

  /** Mount a component under a slot id and ship its first frame. */
  registerComponent(
    slot: string,
    component: DisposableComponent,
    options?: { focusable?: boolean; placement?: WidgetPlacementDto },
  ): void {
    this.disposeSlot(slot, { notify: false });
    this.slots.set(slot, {
      kind: "component",
      component,
      width: DEFAULT_WIDTH,
      version: 0,
      focusable: options?.focusable === true,
      ...(options?.placement !== undefined ? { placement: options.placement } : {}),
      dirty: false,
    });
    this.markDirty(slot);
  }

  /** Mount a static string[] widget (no component). */
  registerStatic(slot: string, lines: string[], options?: { placement?: WidgetPlacementDto }): void {
    this.disposeSlot(slot, { notify: false });
    this.slots.set(slot, {
      kind: "static",
      lines,
      width: DEFAULT_WIDTH,
      version: 0,
      ...(options?.placement !== undefined ? { placement: options.placement } : {}),
      dirty: false,
    });
    this.markDirty(slot);
  }

  has(slot: string): boolean {
    return this.slots.has(slot);
  }

  /** Dispose a slot locally and (by default) tell Rust to drop the leaf. */
  disposeSlot(slot: string, options?: { notify?: boolean }): void {
    const entry = this.slots.get(slot);
    if (entry === undefined) return;
    this.slots.delete(slot);
    if (entry.kind === "component") entry.component.dispose?.();
    if (options?.notify !== false) {
      this.deps.peer.notify("ui/dispose", { slot });
    }
  }

  markDirty(slot: string): void {
    const entry = this.slots.get(slot);
    if (entry === undefined) return;
    entry.dirty = true;
    if (this.flushScheduled) return;
    this.flushScheduled = true;
    queueMicrotask(() => {
      this.flushScheduled = false;
      this.flush();
    });
  }

  private flush(): void {
    for (const [slot, entry] of this.slots) {
      if (!entry.dirty) continue;
      entry.dirty = false;
      this.shipFrame(slot, entry);
    }
  }

  private shipFrame(slot: string, entry: Slot): void {
    let lines: string[];
    try {
      lines = entry.kind === "static" ? entry.lines : entry.component.render(entry.width);
    } catch (error) {
      this.deps.peer.notify("error/extension", {
        extensionPath: "<extension component>",
        event: `render:${slot}`,
        error: error instanceof Error ? error.message : String(error),
      });
      return;
    }
    entry.version += 1;
    this.deps.peer.notify("ui/frame", {
      slot,
      lines,
      version: entry.version,
      wantsKeyRelease: entry.kind === "component" && entry.component.wantsKeyRelease === true,
      focusable: entry.kind === "component" && entry.focusable,
      ...(entry.placement !== undefined ? { placement: entry.placement } : {}),
    });
  }

  // ----- UiBridge surface (host delegates inbound ui/* traffic here) -----

  /** `ui/render {slot,width}`: render synchronously at the given width. */
  render(slot: string, width: number): string[] {
    const entry = this.slots.get(slot) ?? this.resolveDynamicSlot(slot);
    if (entry === undefined) {
      throw new Error(`unknown UI slot: ${slot}`);
    }
    entry.width = width;
    this.terminal.resize(Math.max(width, this.terminal.columns));
    if (entry.kind === "static") return entry.lines;
    return entry.component.render(width);
  }

  /** `ui/input {slot,data}`: forwarded key input for a focused slot. */
  input(slot: string, data: string): void {
    const entry = this.slots.get(slot);
    if (entry === undefined || entry.kind !== "component") return;
    entry.component.handleInput?.(data);
    this.markDirty(slot);
  }

  /** `ui/focus {slot,focused}`: mirror host focus onto the component so it
   * renders its cursor marker exactly like a locally focused component.
   * pi-tui `Focusable` is a `focused: boolean` PROPERTY (tui.ts:104-107);
   * the TUI assigns it directly, so the bridge does the same. */
  focus(slot: string, focused: boolean): void {
    const entry = this.slots.get(slot);
    if (entry === undefined || entry.kind !== "component") return;
    if (isFocusable(entry.component)) {
      entry.component.focused = focused;
      this.markDirty(slot);
    }
  }

  /** `ui/setEditorText` (host→sidecar): update the bridged editor's text. */
  editorSetText(text: string): void {
    const entry = this.slots.get("editor");
    if (entry === undefined || entry.kind !== "component") return;
    const component: unknown = entry.component;
    if (
      component !== null &&
      typeof component === "object" &&
      "setText" in component &&
      typeof component.setText === "function"
    ) {
      component.setText(text);
      this.markDirty("editor");
    }
  }

  dispose(slot: string): void {
    // Initiated by Rust; do not echo a ui/dispose back.
    this.disposeSlot(slot, { notify: false });
    if (slot.startsWith("tool:")) {
      const toolCallId = slot.slice("tool:".length).replace(/:(call|result)$/, "");
      this.toolRecords.delete(toolCallId);
    }
  }

  // ----- terminal input listeners (ctx.ui.onTerminalInput) -----

  private readonly terminalInputListeners: Array<(data: string) => { consume?: boolean; data?: string } | undefined> = [];

  addTerminalInputListener(handler: (data: string) => { consume?: boolean; data?: string } | undefined): () => void {
    this.terminalInputListeners.push(handler);
    if (this.terminalInputListeners.length === 1) {
      this.deps.peer.notify("ui/terminalInputActive", { active: true });
    }
    return () => {
      const index = this.terminalInputListeners.indexOf(handler);
      if (index < 0) return; // Idempotent: already removed.
      this.terminalInputListeners.splice(index, 1);
      if (this.terminalInputListeners.length === 0) {
        this.deps.peer.notify("ui/terminalInputActive", { active: false });
      }
    };
  }

  hasTerminalInputListeners(): boolean {
    return this.terminalInputListeners.length > 0;
  }

  /** `ui/terminal_input`: pi TUI listener semantics — first consumer wins. */
  async terminalInput(data: string): Promise<TerminalInputResultDto> {
    let current = data;
    for (const listener of [...this.terminalInputListeners]) {
      const result = listener(current);
      if (result?.data !== undefined) current = result.data;
      if (result?.consume === true) {
        return { consume: true, ...(current !== data ? { data: current } : {}) };
      }
    }
    return current !== data ? { consume: false, data: current } : { consume: false };
  }

  // ----- tool renderer slots (tool:<id>:call | tool:<id>:result) -----

  recordToolCall(toolCallId: string, tool: RegisteredTool, args: unknown): void {
    this.toolRecords.set(toolCallId, {
      tool,
      args,
      isPartial: false,
      isError: false,
      executionStarted: true,
      state: {},
    });
    if (tool.definition.renderCall !== undefined) {
      this.markDynamicDirty(`tool:${toolCallId}:call`);
    }
  }

  recordToolUpdate(toolCallId: string, partial: AgentToolResult<unknown>): void {
    const record = this.toolRecords.get(toolCallId);
    if (record === undefined) return;
    record.result = partial;
    record.isPartial = true;
    if (record.tool.definition.renderResult !== undefined) {
      this.markDynamicDirty(`tool:${toolCallId}:result`);
    }
  }

  recordToolResult(toolCallId: string, result: AgentToolResult<unknown>, isError: boolean): void {
    const record = this.toolRecords.get(toolCallId);
    if (record === undefined) return;
    record.result = result;
    record.isPartial = false;
    record.isError = isError;
    if (record.tool.definition.renderResult !== undefined) {
      this.markDynamicDirty(`tool:${toolCallId}:result`);
    }
  }

  /** Dynamic slots materialize on first use, then behave like components. */
  private markDynamicDirty(slot: string): void {
    if (!this.slots.has(slot)) {
      const resolved = this.resolveDynamicSlot(slot);
      if (resolved === undefined) return;
    }
    this.markDirty(slot);
  }

  private resolveDynamicSlot(slot: string): Slot | undefined {
    const component = this.buildDynamicComponent(slot);
    if (component === undefined) return undefined;
    const entry: ComponentSlot = {
      kind: "component",
      component,
      width: DEFAULT_WIDTH,
      version: 0,
      focusable: false,
      dirty: false,
    };
    this.slots.set(slot, entry);
    return entry;
  }

  private buildDynamicComponent(slot: string): DisposableComponent | undefined {
    const toolMatch = /^tool:(.+):(call|result)$/.exec(slot);
    if (toolMatch !== null) {
      const [, toolCallId = "", phase = ""] = toolMatch;
      const record = this.toolRecords.get(toolCallId);
      if (record === undefined) return undefined;
      return this.toolRenderComponent(slot, toolCallId, record, phase === "result");
    }
    if (slot.startsWith("entry:")) {
      return this.deps.resolveEntryComponent?.(slot.slice("entry:".length));
    }
    if (slot.startsWith("msg:")) {
      return this.deps.resolveMessageComponent?.(slot.slice("msg:".length));
    }
    return undefined;
  }

  /**
   * A stable leaf component whose render() re-invokes the extension's
   * renderCall/renderResult with a faithful ToolRenderContext (invalidate,
   * lastComponent, shared state).
   */
  private toolRenderComponent(
    slot: string,
    toolCallId: string,
    record: ToolRenderRecord,
    isResult: boolean,
  ): DisposableComponent {
    const bridge = this;
    return {
      invalidate(): void {
        if (isResult) {
          record.lastResultComponent = undefined;
        } else {
          record.lastCallComponent = undefined;
        }
      },
      render(width: number): string[] {
        const context: ToolRenderContext = {
          args: record.args,
          toolCallId,
          invalidate: () => bridge.markDirty(slot),
          lastComponent: isResult ? record.lastResultComponent : record.lastCallComponent,
          state: record.state,
          cwd: bridge.deps.cwd,
          executionStarted: record.executionStarted,
          argsComplete: true,
          isPartial: record.isPartial,
          expanded: bridge.deps.toolsExpanded(),
          showImages: false,
          isError: record.isError,
        };
        const definition = record.tool.definition;
        let component: Component | undefined;
        if (isResult) {
          if (definition.renderResult !== undefined && record.result !== undefined) {
            component = definition.renderResult(
              record.result,
              { expanded: context.expanded, isPartial: record.isPartial },
              activeTheme,
              context,
            );
          }
          record.lastResultComponent = component;
        } else {
          component = definition.renderCall?.(record.args, activeTheme, context);
          record.lastCallComponent = component;
        }
        return component?.render(width) ?? [];
      },
    };
  }
}
