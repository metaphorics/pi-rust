/**
 * RPC-proxy implementations of pi's extension action surfaces:
 * `ExtensionActions` (pi.* API), `ExtensionContextActions` (ctx.* in event
 * handlers), and `ExtensionCommandContextActions` (ctx.* in commands).
 *
 * Classification (normative, architecture packet §3):
 * - ASYNC-REQUEST members await an RPC round-trip;
 * - SYNC-VOID members send a notification and optimistically apply their
 *   deterministic local effect to the mirrors;
 * - SYNC-GETTER members are served from the state/session mirrors.
 */

import type {
  BuildSystemPromptOptions,
  CompactOptions,
  ExtensionActions,
  ExtensionCommandContext,
  ExtensionCommandContextActions,
  ExtensionContextActions,
  SessionManager,
  SlashCommandInfo,
  ToolInfo,
} from "@earendil-works/pi-coding-agent";
import type { Api, Model, ThinkingLevel } from "@earendil-works/pi-ai";

import type { ReplacedSessionContext } from "./pi-internal.ts";

import { fromWire, toWire } from "./protocol.ts";
import type { JsonObject, JsonValue } from "./protocol.ts";
import type { RpcPeer } from "./rpc.ts";
import type { SessionMirror } from "./session-mirror.ts";
import type { StateMirror } from "./state-mirror.ts";

/** A pending `setup`/`withSession` callback awaiting its `session/setup`. */
export type PendingSetup =
  | { kind: "setup"; run: (sessionManager: SessionManager) => Promise<void> }
  | { kind: "withSession"; run: (ctx: ReplacedSessionContext) => Promise<void> };

export interface BridgedActionDeps {
  peer: RpcPeer;
  state: StateMirror;
  session: SessionMirror;
  cwd: string;
  /** Full command catalog (extension + host commands); bound by the runtime. */
  getCommands: () => SlashCommandInfo[];
  /** Fresh command context for ReplacedSessionContext construction. */
  createCommandContext: () => ExtensionCommandContext;
}

export interface BridgedActions {
  actions: ExtensionActions;
  contextActions: ExtensionContextActions;
  commandContextActions: ExtensionCommandContextActions;
  pendingSetups: Map<string, PendingSetup>;
  /**
   * Fired by the host when a manual `session_compact` event arrives.
   * Resolves exactly ONE pending `ctx.compact()` callback (FIFO).
   */
  notifySessionCompact: (compactionEntry: JsonObject, reason: string) => void;
  /**
   * Fired by the host when a manual compaction observably fails (a
   * `session_before_compact` handler returned `cancel: true`). Rejects
   * exactly ONE pending `ctx.compact()` callback (FIFO).
   */
  failPendingCompact: (error: Error) => void;
  /** Turn-signal tracker; the host feeds idle transitions into it. */
  turnSignal: TurnSignalTracker;
}

/**
 * Tracks the per-turn AbortSignal that `ctx.signal` exposes.
 *
 * pi returns the live turn signal while streaming and `undefined` when idle.
 * Across the bridge the signal aborts only on an observed abort (local
 * `ctx.abort()`); a normally-completed turn simply retires its controller.
 */
export class TurnSignalTracker {
  private controller: AbortController | undefined;

  /** Called on every observed idle transition (state blocks and updates). */
  setIdle(idle: boolean): void {
    if (idle) {
      this.controller = undefined;
    } else if (this.controller === undefined) {
      this.controller = new AbortController();
    }
  }

  get signal(): AbortSignal | undefined {
    return this.controller?.signal;
  }

  abortLocal(): void {
    this.controller?.abort();
    this.controller = undefined;
  }
}

let nextSetupToken = 1;

export function createBridgedActions(deps: BridgedActionDeps): BridgedActions {
  const { peer, state, session } = deps;
  const pendingSetups = new Map<string, PendingSetup>();
  const turnSignal = new TurnSignalTracker();
  const pendingCompacts: CompactOptions[] = [];

  const actions: ExtensionActions = {
    sendMessage: (message, options) => {
      peer.notify("action/sendMessage", {
        message: message as JsonValue,
        ...(options?.triggerTurn !== undefined ? { triggerTurn: options.triggerTurn } : {}),
        ...(options?.deliverAs !== undefined ? { deliverAs: options.deliverAs } : {}),
      });
    },
    sendUserMessage: (content, options) => {
      peer.notify("action/sendUserMessage", {
        content: content as JsonValue,
        ...(options?.deliverAs !== undefined ? { deliverAs: options.deliverAs } : {}),
      });
    },
    appendEntry: (customType, data) => {
      session.optimisticCustomEntry(customType, data);
      peer.notify("action/appendEntry", {
        customType,
        ...(data !== undefined ? { data: data as JsonValue } : {}),
      });
    },
    setSessionName: (name) => {
      session.optimisticSessionInfo(name);
      state.setSessionName(name);
      peer.notify("action/setSessionName", { name });
    },
    getSessionName: () => session.sessionManager.getSessionName() ?? state.current.sessionName,
    setLabel: (entryId, label) => {
      session.optimisticLabelChange(entryId, label);
      peer.notify("action/setLabel", { entryId, ...(label !== undefined ? { label } : {}) });
    },
    getActiveTools: () => state.current.activeTools,
    getAllTools: () =>
      state.current.allTools.map((tool): ToolInfo => {
        const info = fromWire<ToolInfo>(tool);
        // The wire flattens pi's promptGuidelines string[] to one string.
        if (typeof tool.promptGuidelines === "string") {
          return { ...info, promptGuidelines: tool.promptGuidelines.split("\n") };
        }
        return info;
      }),
    setActiveTools: (toolNames) => {
      state.setActiveTools(toolNames);
      peer.notify("action/setActiveTools", { toolNames });
    },
    refreshTools: () => {
      peer.notify("action/refreshTools", {});
    },
    getCommands: () => deps.getCommands(),
    setModel: async (model) => {
      const ok = await peer.request("action/setModel", { model: toWire(model) });
      return ok === true;
    },
    getThinkingLevel: () => state.current.thinkingLevel as ThinkingLevel,
    setThinkingLevel: (level) => {
      state.setThinkingLevel(level);
      peer.notify("action/setThinkingLevel", { level });
    },
  };

  const contextActions: ExtensionContextActions = {
    getModel: () => (state.model === undefined ? undefined : fromWire<Model<Api>>(state.model)),
    isIdle: () => state.idle,
    isProjectTrusted: () => state.current.projectTrusted,
    getSignal: () => turnSignal.signal,
    abort: () => {
      peer.notify("action/abort", {});
      turnSignal.abortLocal();
    },
    hasPendingMessages: () => state.current.pendingMessages,
    shutdown: () => {
      peer.notify("action/shutdown", {});
    },
    getContextUsage: () => state.current.contextUsage,
    compact: (options) => {
      // Every compact() call queues one FIFO entry (even option-less calls)
      // so completions/failures consume pendings in call order.
      pendingCompacts.push(options ?? {});
      peer.notify("action/compact", {
        ...(options?.customInstructions !== undefined
          ? { options: { customInstructions: options.customInstructions } }
          : {}),
      });
    },
    getSystemPrompt: () => state.current.systemPrompt,
    getSystemPromptOptions: () =>
      state.current.systemPromptOptions === undefined
        ? { cwd: deps.cwd }
        : fromWire<BuildSystemPromptOptions>(state.current.systemPromptOptions),
  };

  const commandContextActions: ExtensionCommandContextActions = {
    waitForIdle: async () => {
      await peer.request("action/waitForIdle", {});
    },
    newSession: async (options) => {
      const params: JsonObject = {};
      if (options?.parentSession !== undefined) params["parentSession"] = options.parentSession;
      if (options?.setup !== undefined) {
        const token = `setup-${nextSetupToken++}`;
        pendingSetups.set(token, { kind: "setup", run: options.setup });
        params["setupToken"] = token;
      }
      if (options?.withSession !== undefined) {
        const token = `with-${nextSetupToken++}`;
        pendingSetups.set(token, { kind: "withSession", run: options.withSession });
        params["withSessionToken"] = token;
      }
      return decodeCancelled(await peer.request("action/newSession", params));
    },
    fork: async (entryId, options) => {
      const params: JsonObject = { entryId };
      if (options?.position !== undefined) params["position"] = options.position;
      if (options?.withSession !== undefined) {
        const token = `with-${nextSetupToken++}`;
        pendingSetups.set(token, { kind: "withSession", run: options.withSession });
        params["withSessionToken"] = token;
      }
      return decodeCancelled(await peer.request("action/fork", params));
    },
    navigateTree: async (targetId, options) => {
      const params: JsonObject = { targetId };
      if (options?.summarize !== undefined) params["summarize"] = options.summarize;
      if (options?.customInstructions !== undefined) params["customInstructions"] = options.customInstructions;
      if (options?.replaceInstructions !== undefined) params["replaceInstructions"] = options.replaceInstructions;
      if (options?.label !== undefined) params["label"] = options.label;
      return decodeCancelled(await peer.request("action/navigateTree", params));
    },
    switchSession: async (sessionPath, options) => {
      const params: JsonObject = { sessionPath };
      if (options?.withSession !== undefined) {
        const token = `with-${nextSetupToken++}`;
        pendingSetups.set(token, { kind: "withSession", run: options.withSession });
        params["withSessionToken"] = token;
      }
      return decodeCancelled(await peer.request("action/switchSession", params));
    },
    reload: async () => {
      await peer.request("action/reload", {});
    },
  };

  // ctx.compact() runs as a manual compaction on the host (oracle:
  // agent-session.ts compact() -> reason "manual"), so only manual outcomes
  // consume pendings; threshold/overflow auto-compactions never do. Note
  // session_compact's fromExtension flags an extension-SUPPLIED summary
  // (session_before_compact result), not an extension-TRIGGERED compaction,
  // so it cannot gate correlation. Residual ambiguity: a user /compact
  // interleaved with a pending ctx.compact() is indistinguishable without a
  // wire correlation id (protocol v1 gap).
  const notifySessionCompact = (compactionEntry: JsonObject, reason: string): void => {
    if (reason !== "manual") return;
    const options = pendingCompacts.shift();
    if (options === undefined) return;
    try {
      options.onComplete?.({
        summary: typeof compactionEntry["summary"] === "string" ? compactionEntry["summary"] : "",
        firstKeptEntryId:
          typeof compactionEntry["firstKeptEntryId"] === "string" ? compactionEntry["firstKeptEntryId"] : "",
        tokensBefore:
          typeof compactionEntry["tokensBefore"] === "number" ? compactionEntry["tokensBefore"] : 0,
        ...(typeof compactionEntry["estimatedTokensAfter"] === "number"
          ? { estimatedTokensAfter: compactionEntry["estimatedTokensAfter"] }
          : {}),
        details: compactionEntry["details"],
      });
    } catch {
      // Extension callback errors must not break event dispatch.
    }
  };

  const failPendingCompact = (error: Error): void => {
    const options = pendingCompacts.shift();
    if (options === undefined) return;
    try {
      options.onError?.(error);
    } catch {
      // Extension callback errors must not break event dispatch.
    }
  };

  return {
    actions,
    contextActions,
    commandContextActions,
    pendingSetups,
    notifySessionCompact,
    failPendingCompact,
    turnSignal,
  };
}

function decodeCancelled(ok: JsonValue): { cancelled: boolean } {
  if (typeof ok === "object" && ok !== null && !Array.isArray(ok) && typeof ok["cancelled"] === "boolean") {
    return { cancelled: ok["cancelled"] };
  }
  return { cancelled: false };
}

/**
 * Build the ReplacedSessionContext handed to `withSession()` callbacks: a
 * fresh command context whose message sends are token-scoped RPC requests
 * (Rust holds the replaced-session op while this runs).
 */
export function createReplacedSessionContext(
  base: ExtensionCommandContext,
  peer: RpcPeer,
): ReplacedSessionContext {
  return {
    ...base,
    sendMessage: async (message, options) => {
      await peer.request("action/replaced/sendMessage", {
        message: message as JsonValue,
        ...(options?.triggerTurn !== undefined ? { triggerTurn: options.triggerTurn } : {}),
        ...(options?.deliverAs !== undefined ? { deliverAs: options.deliverAs } : {}),
      });
    },
    sendUserMessage: async (content, options) => {
      await peer.request("action/replaced/sendUserMessage", {
        content: content as JsonValue,
        ...(options?.deliverAs !== undefined ? { deliverAs: options.deliverAs } : {}),
      });
    },
  };
}
