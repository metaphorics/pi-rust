/**
 * In-process test harness: a fake Rust-side peer wired directly to the
 * sidecar host. No subprocess, no timers — both peers exchange frames
 * synchronously and tests drive everything through awaited requests.
 */

import { readFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { attachHost } from "../src/host.ts";
import type { SidecarHost } from "../src/host.ts";
import type {
  InitParamsDto,
  InitializedParamsDto,
  JsonObject,
  JsonValue,
  NotificationMethod,
  StateBlockDto,
} from "../src/protocol.ts";
import { fromWire } from "../src/protocol.ts";
import { RpcPeer } from "../src/rpc.ts";

const DARK_THEME_PATH = join(
  import.meta.dir,
  "..",
  "node_modules",
  "@earendil-works",
  "pi-coding-agent",
  "dist",
  "modes",
  "interactive",
  "theme",
  "dark.json",
);

/** pi's real bundled dark theme (a valid, complete theme JSON). */
export function darkThemeJson(): JsonValue {
  return JSON.parse(readFileSync(DARK_THEME_PATH, "utf-8")) as JsonValue;
}

export function makeStateBlock(overrides?: Partial<StateBlockDto>): StateBlockDto {
  return {
    idle: true,
    projectTrusted: true,
    pendingMessages: false,
    activeTools: [],
    allTools: [],
    commands: [],
    thinkingLevel: "medium",
    systemPrompt: "test system prompt",
    flagValues: {},
    editorText: "",
    toolsExpanded: false,
    theme: { name: "dark", json: darkThemeJson() },
    ...overrides,
  };
}

export function makeInitParams(overrides?: Partial<InitParamsDto>): InitParamsDto {
  const dir = mkdtempSync(join(tmpdir(), "pi-sidecar-test-"));
  return {
    cwd: dir,
    agentDir: join(dir, ".pi", "agent"),
    sessionDir: join(dir, ".pi", "agent", "sessions"),
    configuredPaths: [],
    mode: "tui",
    hasUi: true,
    flagValues: {},
    theme: { name: "dark", json: darkThemeJson() },
    session: {
      epoch: 0,
      sessionFile: join(dir, "session.jsonl"),
      entries: [],
      leafId: null,
    },
    state: makeStateBlock(),
    ...overrides,
  };
}

export interface RustSide {
  peer: RpcPeer;
  host: SidecarHost;
  /** Every notification the sidecar sent, in order. */
  notifications: Array<{ method: NotificationMethod; params: JsonValue }>;
  /** Raw frames the sidecar wrote (protocol-purity assertions). */
  rawFrames: Uint8Array[];
  transportErrors: Error[];
  /** Await lifecycle/init + return the initialized payload. */
  init(params: InitParamsDto): Promise<InitializedParamsDto>;
  notificationsOf(method: NotificationMethod): JsonValue[];
  waitForNotification(method: NotificationMethod, alreadySeen?: number): Promise<JsonValue>;
}

export function createTestBridge(): RustSide {
  const notifications: Array<{ method: NotificationMethod; params: JsonValue }> = [];
  const rawFrames: Uint8Array[] = [];
  const transportErrors: Error[] = [];
  const waiters: Array<{ method: NotificationMethod; minCount: number; resolve: (params: JsonValue) => void }> = [];

  let sidecarPeer: RpcPeer;
  const rustPeer = new RpcPeer({
    write: (bytes) => {
      sidecarPeer.feed(bytes);
    },
    onTransportError: (error) => transportErrors.push(error),
  });
  sidecarPeer = new RpcPeer({
    write: (bytes) => {
      rawFrames.push(bytes);
      rustPeer.feed(bytes);
    },
    onTransportError: (error) => transportErrors.push(error),
  });

  // The fake Rust side records EVERY sidecar notification method.
  const RECORDED: NotificationMethod[] = [
    "lifecycle/hello",
    "lifecycle/initialized",
    "lifecycle/pong",
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
    "ui/dispose",
    "ui/done",
    "ui/overlay",
    "tool/update",
    "provider/register",
    "provider/unregister",
    "provider/event",
    "error/extension",
  ];
  for (const method of RECORDED) {
    rustPeer.onNotification(method, (params) => {
      notifications.push({ method, params });
      for (let i = waiters.length - 1; i >= 0; i--) {
        const waiter = waiters[i];
        if (waiter !== undefined && waiter.method === method) {
          const count = notifications.filter((entry) => entry.method === method).length;
          if (count >= waiter.minCount) {
            waiters.splice(i, 1);
            waiter.resolve(params);
          }
        }
      }
    });
  }

  const host = attachHost({ peer: sidecarPeer, bunVersion: "test" });

  return {
    peer: rustPeer,
    host,
    notifications,
    rawFrames,
    transportErrors,
    init: async (params) => {
      host.sendHello();
      await rustPeer.request("lifecycle/init", fromWire<JsonObject>(params));
      const initialized = notifications.find((entry) => entry.method === "lifecycle/initialized");
      if (initialized === undefined) {
        throw new Error("sidecar never sent lifecycle/initialized");
      }
      return fromWire<InitializedParamsDto>(initialized.params);
    },
    notificationsOf: (method) =>
      notifications.filter((entry) => entry.method === method).map((entry) => entry.params),
    waitForNotification: (method, alreadySeen = 0) => {
      const matching = notifications.filter((entry) => entry.method === method);
      const target = matching[alreadySeen];
      if (target !== undefined) {
        return Promise.resolve(target.params);
      }
      const { promise, resolve } = Promise.withResolvers<JsonValue>();
      waiters.push({ method, minCount: alreadySeen + 1, resolve });
      return promise;
    },
  };
}

export const FIXTURES_DIR = join(import.meta.dir, "fixtures", "extensions");
