/**
 * ExtensionUIContext implementation over the bridge:
 * - value dialogs (select/confirm/input/editor) are RPC requests rendered by
 *   Rust natives — they NEVER render in-sidecar;
 * - void setters are notifications (with optimistic mirror applies);
 * - component-factory members instantiate REAL npm pi-tui components
 *   in-sidecar and ship line frames through the FrameBridge;
 * - sync getters are served from the state mirror / pi's theme module.
 */

import type {
  AutocompleteItem,
  AutocompleteProvider,
  Component,
  OverlayHandle,
  OverlayOptions,
} from "@earendil-works/pi-tui";
import type {
  AutocompleteProviderFactory,
  ExtensionUIContext,
  ExtensionUIDialogOptions,
  ExtensionWidgetOptions,
  TerminalInputHandler,
  WorkingIndicatorOptions,
} from "@earendil-works/pi-coding-agent";
import type { Theme } from "@earendil-works/pi-coding-agent";
import type { EditorFactory } from "./pi-internal.ts";

import { FrameBridge } from "./frames.ts";
import type { DisposableComponent } from "./frames.ts";
import type { UiBridge } from "./host.ts";
import {
  FooterDataProvider,
  activeTheme,
  getAvailableThemesWithPaths,
  getEditorTheme,
  getThemeByName,
  loadThemeFromPath,
  setThemeInstance,
} from "./pi-internal.ts";
import { fromWire, toWire } from "./protocol.ts";
import type { JsonObject, JsonValue, ThemeCatalogEntry } from "./protocol.ts";
import { RpcCancelledError } from "./rpc.ts";
import type { RpcPeer } from "./rpc.ts";
import type { SidecarRuntime } from "./runtime.ts";

function dialogParams(base: JsonObject, opts: ExtensionUIDialogOptions | undefined): JsonObject {
  return opts?.timeout !== undefined ? { ...base, timeout: opts.timeout } : base;
}

export interface CreatedUi {
  context: ExtensionUIContext;
  bridge: UiBridge;
}

export function createUi(runtime: SidecarRuntime): CreatedUi {
  const peer: RpcPeer = runtime.peer;
  const state = runtime.state;

  const bridge = new FrameBridge({
    peer,
    cwd: runtime.cwd,
    toolsExpanded: () => state.current.toolsExpanded,
    resolveEntryComponent: (entryId) => {
      const entry = runtime.session.sessionManager.getEntry(entryId);
      if (entry === undefined || entry.type !== "custom") return undefined;
      const renderer = runtime.runner.getEntryRenderer(entry.customType);
      return renderer?.(entry, { expanded: state.current.toolsExpanded }, activeTheme);
    },
    resolveMessageComponent: (messageKey) => {
      const entry = runtime.session.sessionManager.getEntry(messageKey);
      if (entry === undefined || entry.type !== "custom_message") return undefined;
      const renderer = runtime.runner.getMessageRenderer(entry.customType);
      // CustomMessageEntry carries the CustomMessage fields the renderer needs.
      return renderer?.(
        {
          role: "custom",
          customType: entry.customType,
          content: entry.content,
          display: entry.display,
          details: entry.details,
          timestamp: Date.parse(entry.timestamp) || Date.now(),
        },
        { expanded: state.current.toolsExpanded },
        activeTheme,
      );
    },
  });

  // Real footer data (git branch watching runs in-sidecar on the same repo).
  const footerData = new FooterDataProvider(runtime.cwd);


  // Theme catalog: Rust is authoritative; fetched once at boot, refreshed on
  // demand. Sync getters serve the cache merged with pi's local catalog.
  let hostThemeCatalog: ThemeCatalogEntry[] | undefined;
  void peer
    .request("ui/getAllThemes", {})
    .then((catalog) => {
      hostThemeCatalog = fromWire<ThemeCatalogEntry[]>(catalog);
    })
    .catch(() => {
      // Host catalog unavailable; local catalog remains the fallback.
    });

  const autocompleteFactories: AutocompleteProviderFactory[] = [];
  let editorFactory: EditorFactory | undefined;
  let customSlotCounter = 0;

  const dialogRequest = async (
    method: "ui/select" | "ui/confirm" | "ui/input" | "ui/editor",
    params: JsonObject,
    opts?: ExtensionUIDialogOptions,
  ): Promise<JsonValue | undefined> => {
    try {
      return await peer.request(method, dialogParams(params, opts), {
        ...(opts?.signal !== undefined ? { signal: opts.signal } : {}),
      });
    } catch (error) {
      if (error instanceof RpcCancelledError) return undefined;
      throw error;
    }
  };

  const context: ExtensionUIContext = {
    async select(title, options, opts) {
      const result = await dialogRequest("ui/select", { title, options }, opts);
      return typeof result === "string" ? result : undefined;
    },
    async confirm(title, message, opts) {
      const result = await dialogRequest("ui/confirm", { title, message }, opts);
      return result === true;
    },
    async input(title, placeholder, opts) {
      const result = await dialogRequest(
        "ui/input",
        placeholder !== undefined ? { title, placeholder } : { title },
        opts,
      );
      return typeof result === "string" ? result : undefined;
    },
    async editor(title, prefill) {
      const result = await dialogRequest("ui/editor", { title, text: prefill ?? "" });
      return typeof result === "string" ? result : undefined;
    },
    notify(message, type) {
      peer.notify("ui/notify", { message, level: type ?? "info" });
    },
    onTerminalInput(handler: TerminalInputHandler) {
      return bridge.addTerminalInputListener(handler);
    },
    setStatus(key, text) {
      footerData.setExtensionStatus(key, text);
      // Oracle: interactive-mode setExtensionStatus() requests a render
      // after the provider mutation; the bridged footer leaf must repaint
      // too (markDirty no-ops until a footer is registered).
      bridge.markDirty("footer");
      peer.notify("ui/setStatus", text !== undefined ? { key, value: text } : { key });
    },
    setWorkingMessage(message) {
      peer.notify("ui/setWorkingMessage", message !== undefined ? { text: message } : {});
    },
    setWorkingVisible(visible) {
      peer.notify("ui/setWorkingVisible", { visible });
    },
    setWorkingIndicator(options?: WorkingIndicatorOptions) {
      peer.notify("ui/setWorkingIndicator", options !== undefined ? { options: toWire(options) } : {});
    },
    setHiddenThinkingLabel(label) {
      peer.notify("ui/setHiddenThinkingLabel", label !== undefined ? { text: label } : {});
    },
    setWidget(key: string, content: unknown, options?: ExtensionWidgetOptions) {
      const slot = `widget:${key}`;
      const placement = options?.placement;
      if (content === undefined) {
        bridge.disposeSlot(slot);
        return;
      }
      if (Array.isArray(content)) {
        bridge.registerStatic(slot, content.map((line) => String(line)), {
          ...(placement !== undefined ? { placement } : {}),
        });
        return;
      }
      const factory = content as (tui: typeof bridge.tui, theme: Theme) => DisposableComponent;
      bridge.registerComponent(slot, factory(bridge.tui, activeTheme), {
        ...(placement !== undefined ? { placement } : {}),
      });
    },
    setFooter(factory) {
      if (factory === undefined) {
        bridge.disposeSlot("footer");
        return;
      }
      bridge.registerComponent("footer", factory(bridge.tui, activeTheme, footerData));
    },
    setHeader(factory) {
      if (factory === undefined) {
        bridge.disposeSlot("header");
        return;
      }
      bridge.registerComponent("header", factory(bridge.tui, activeTheme));
    },
    setTitle(title) {
      peer.notify("ui/setTitle", { text: title });
    },
    async custom<T>(
      factory: (
        tui: typeof bridge.tui,
        theme: Theme,
        keybindings: typeof runtime.keybindings,
        done: (result: T) => void,
      ) => (Component & { dispose?(): void }) | Promise<Component & { dispose?(): void }>,
      options?: {
        overlay?: boolean;
        overlayOptions?: OverlayOptions | (() => OverlayOptions);
        onHandle?: (handle: OverlayHandle) => void;
      },
    ): Promise<T> {
      const slot = `custom:${++customSlotCounter}`;
      const { promise, resolve } = Promise.withResolvers<T>();
      let settled = false;
      const done = (result: T): void => {
        if (settled) return;
        settled = true;
        peer.notify("ui/done", { slot, result: toWire(result) });
        resolve(result);
      };
      const component = await factory(bridge.tui, activeTheme, runtime.keybindings, done);
      bridge.registerComponent(slot, component, { focusable: true });

      const overlay = options?.overlay === true;
      const resolveOverlayOptions = (): OverlayOptions => {
        const overlayOptions = options?.overlayOptions;
        return typeof overlayOptions === "function" ? overlayOptions() : (overlayOptions ?? {});
      };
      if (overlay) {
        if (options?.onHandle !== undefined) {
          // The REAL headless-TUI overlay handle keeps pi's focus/visibility
          // semantics; every observable change re-ships the overlay options
          // (with `hidden`) so Rust mirrors the state.
          const handle = bridge.tui.showOverlay(component, resolveOverlayOptions());
          const shipOverlayState = () => {
            peer.notify("ui/overlay", {
              slot,
              options: toWire({ ...resolveOverlayOptions(), hidden: handle.isHidden() }),
            });
          };
          options.onHandle({
            hide: () => {
              handle.hide();
              bridge.disposeSlot(slot);
            },
            setHidden: (hidden: boolean) => {
              handle.setHidden(hidden);
              shipOverlayState();
            },
            isHidden: () => handle.isHidden(),
            focus: () => {
              handle.focus();
              shipOverlayState();
            },
            unfocus: (unfocusOptions) => {
              handle.unfocus(unfocusOptions);
              shipOverlayState();
            },
            isFocused: () => handle.isFocused(),
          });
        }
        peer.notify("ui/overlay", { slot, options: toWire(resolveOverlayOptions()) });
      }

      // Rust hosts the dialog; its response finalizes the slot's lifetime.
      void peer
        .request("ui/custom", { slot, overlay, overlayOptions: toWire(resolveOverlayOptions()) })
        .catch(() => undefined)
        .finally(() => {
          bridge.disposeSlot(slot, { notify: false });
        });
      return promise;
    },
    pasteToEditor(text) {
      peer.notify("ui/pasteToEditor", { text });
    },
    setEditorText(text) {
      state.setEditorText(text);
      peer.notify("ui/setEditorText", { text });
    },
    getEditorText() {
      return state.current.editorText;
    },
    addAutocompleteProvider(factory) {
      autocompleteFactories.push(factory);
    },
    setEditorComponent(factory) {
      editorFactory = factory;
      if (factory === undefined) {
        bridge.disposeSlot("editor");
        return;
      }
      bridge.registerComponent("editor", factory(bridge.tui, getEditorTheme(), runtime.keybindings), {
        focusable: true,
      });
    },
    getEditorComponent() {
      return editorFactory;
    },
    get theme(): Theme {
      return activeTheme;
    },
    getAllThemes() {
      if (hostThemeCatalog !== undefined) {
        return hostThemeCatalog.map((entry) => ({ name: entry.name, path: entry.path }));
      }
      return getAvailableThemesWithPaths();
    },
    getTheme(name) {
      const hosted = hostThemeCatalog?.find((entry) => entry.name === name);
      if (hosted?.path !== undefined) {
        try {
          return loadThemeFromPath(hosted.path);
        } catch {
          return undefined;
        }
      }
      return getThemeByName(name);
    },
    setTheme(theme) {
      if (typeof theme === "string") {
        const resolved = context.getTheme(theme);
        if (resolved === undefined) {
          return { success: false, error: `Theme not found: ${theme}` };
        }
        setThemeInstance(resolved);
        peer.notify("ui/setTheme", { theme });
        return { success: true };
      }
      // A Theme instance applies locally; Rust is told the name when known.
      setThemeInstance(theme);
      if (theme.name !== undefined) {
        peer.notify("ui/setTheme", { theme: theme.name });
      }
      return { success: true };
    },
    getToolsExpanded() {
      return state.current.toolsExpanded;
    },
    setToolsExpanded(expanded) {
      state.setToolsExpanded(expanded);
      peer.notify("ui/setToolsExpanded", { visible: expanded });
    },
  };

  const uiBridge: UiBridge = {
    render: (slot, width) => bridge.render(slot, width),
    input: (slot, data) => {
      bridge.input(slot, data);
    },
    dispose: (slot) => {
      bridge.dispose(slot);
    },
    terminalInput: (data) => bridge.terminalInput(data),
    autocomplete: (text, cursor, commandName) =>
      runAutocomplete(runtime, autocompleteFactories, text, cursor, commandName),
    recordToolCall: (toolCallId, toolName, args) => {
      const tool = runtime.runner
        .getAllRegisteredTools()
        .find((registered) => registered.definition.name === toolName);
      if (tool !== undefined) bridge.recordToolCall(toolCallId, tool, args);
    },
    recordToolUpdate: (toolCallId, partial) => {
      bridge.recordToolUpdate(toolCallId, fromWire(partial));
    },
    recordToolResult: (toolCallId, result, isError) => {
      bridge.recordToolResult(toolCallId, fromWire(result), isError);
    },
  };

  return { context, bridge: uiBridge };
}

/**
 * Build the autocomplete chain for a `ui/autocomplete` request: command
 * argument completions when a command is named, otherwise the stacked
 * extension providers over an empty base.
 */
export async function runAutocomplete(
  runtime: SidecarRuntime,
  factories: AutocompleteProviderFactory[],
  text: string,
  cursor: number,
  commandName: string | undefined,
): Promise<JsonValue> {
  if (commandName !== undefined) {
    const command = runtime.runner.getCommand(commandName);
    const items = await command?.getArgumentCompletions?.(text);
    return toWire(items ?? null);
  }
  if (factories.length === 0) return null;
  const base: AutocompleteProvider = {
    getSuggestions: async () => null,
    applyCompletion: (lines, cursorLine, cursorCol, item: AutocompleteItem, prefix) => {
      const line = lines[cursorLine] ?? "";
      const before = line.slice(0, cursorCol - prefix.length);
      const after = line.slice(cursorCol);
      const nextLines = [...lines];
      nextLines[cursorLine] = `${before}${item.value}${after}`;
      return { lines: nextLines, cursorLine, cursorCol: before.length + item.value.length };
    },
  };
  let provider = base;
  for (const factory of factories) {
    provider = factory(provider);
  }
  const suggestions = await provider.getSuggestions([text], 0, cursor, {
    signal: new AbortController().signal,
  });
  return toWire(suggestions ?? null);
}
