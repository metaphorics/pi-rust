/**
 * Sidecar runtime — boots pi's REAL extension pipeline:
 * `discoverAndLoadExtensions` (jiti + aliases, so extension imports of
 * `@earendil-works/pi-coding-agent` resolve to the single pinned copy) and
 * `ExtensionRunner` bound to RPC-proxy actions. Nothing here reimplements
 * loader/runner semantics; this module only wires and reports.
 */

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import {
  AuthStorage,
  DefaultResourceLoader,
  ExtensionRunner,
  ModelRegistry,
  createEventBus,
  createSyntheticSourceInfo,
  discoverAndLoadExtensions,
} from "@earendil-works/pi-coding-agent";
import type {
  EventBusController,
  Extension,
  ExtensionError,
  ExtensionUIContext,
  LoadExtensionsResult,
  ProviderConfig,
  RegisteredTool,
  SlashCommandInfo,
} from "@earendil-works/pi-coding-agent";
import {
  KeybindingsManager,
  loadExtensions,
  loadThemeFromPath,
  setThemeInstance,
} from "./pi-internal.ts";
import type { KeybindingsConfig } from "./pi-internal.ts";

import { createBridgedActions, createReplacedSessionContext } from "./actions.ts";
import type { BridgedActions } from "./actions.ts";
import { toWire } from "./protocol.ts";
import type {
  ExtensionErrorDto,
  InitParamsDto,
  JsonValue,
  RegistrationsDto,
  ThemeCatalogEntry,
  ThemeDto,
} from "./protocol.ts";
import type { RpcPeer } from "./rpc.ts";
import { SessionMirror } from "./session-mirror.ts";
import { StateMirror } from "./state-mirror.ts";

export interface RegisteredProvider {
  config: ProviderConfig;
  extensionPath?: string;
}

export interface SidecarRuntime {
  runner: ExtensionRunner;
  loadResult: LoadExtensionsResult;
  bridged: BridgedActions;
  state: StateMirror;
  session: SessionMirror;
  peer: RpcPeer;
  providers: Map<string, RegisteredProvider>;
  keybindingsConfig: KeybindingsConfig;
  keybindings: KeybindingsManager;
  eventBus: EventBusController;
  mode: InitParamsDto["mode"];
  hasUi: boolean;
  cwd: string;
  /** Host theme catalog fetched at boot (ui/getAllThemes); undefined when
   * the host declined or the mode has no UI. */
  hostThemeCatalog: ThemeCatalogEntry[] | undefined;
  loadErrors: ExtensionErrorDto[];
  /** Re-runs `loadExtensions` for extra paths (lifecycle/load). */
  loadMore: (paths: string[]) => Promise<{ registrations: RegistrationsDto; errors: ExtensionErrorDto[] }>;
  registrations: () => RegistrationsDto;
  subscribedEvents: () => string[];
}

export interface BootOptions {
  init: InitParamsDto;
  peer: RpcPeer;
  /** Host theme catalog fetched before boot (host is authoritative). */
  hostThemeCatalog?: ThemeCatalogEntry[];
  /** UI context factory (C4). When absent/declining the runner keeps its no-op UI. */
  uiContext?: (runtime: SidecarRuntime) => ExtensionUIContext | undefined;
}

/**
 * Apply a wire theme via pi's own loader (validation included). Throws on
 * invalid data — the caller reports it; a previous theme stays active.
 *
 * One sidecar-owned temp file is reused for every application (theme JSON
 * arrives over the wire; pi's loader only reads from disk).
 */
let themeTempPath: string | undefined;

export function applyWireTheme(theme: ThemeDto): void {
  if (themeTempPath === undefined) {
    themeTempPath = join(mkdtempSync(join(tmpdir(), "pi-sidecar-theme-")), "theme.json");
    process.on("exit", () => {
      try {
        rmSync(dirname(themeTempPath ?? ""), { recursive: true, force: true });
      } catch {
        // Temp cleanup is best-effort.
      }
    });
  }
  writeFileSync(themeTempPath, JSON.stringify(theme.json));
  setThemeInstance(loadThemeFromPath(themeTempPath));
}

export async function bootRuntime(options: BootOptions): Promise<SidecarRuntime> {
  const { init, peer } = options;
  const eventBus = createEventBus();

  // Unmodified extensions (and pi's own modules) resolve the agent dir via
  // config env overrides, not through our constructor arguments — pin both
  // to the host's directories before any extension code runs.
  process.env["PI_CODING_AGENT_DIR"] = init.agentDir;
  process.env["PI_CODING_AGENT_SESSION_DIR"] = init.sessionDir;

  const loadResult = await discoverAndLoadExtensions(init.configuredPaths, init.cwd, init.agentDir, eventBus);

  const state = new StateMirror(init.state);
  // Every observed theme change (event state blocks and state/update alike)
  // re-applies pi's global theme so factory `theme` params style identically.
  state.onThemeChange = (theme) => {
    try {
      applyWireTheme(theme);
    } catch (error) {
      peer.notify("error/extension", {
        extensionPath: "<sidecar>",
        event: "theme",
        error: error instanceof Error ? error.message : String(error),
      });
    }
  };
  const session = new SessionMirror(init.cwd, init.session);
  session.onStale = (expected, received) => {
    peer.notify("error/extension", {
      extensionPath: "<sidecar>",
      event: "session_sync",
      error: `incremental session/sync epoch ${received} arrived out of order (expected ${expected}); mirror kept until full resync`,
    });
  };

  const authStorage = AuthStorage.create(join(init.agentDir, "auth.json"));
  const modelRegistry = ModelRegistry.create(authStorage, join(init.agentDir, "models.json"));
  const keybindings = KeybindingsManager.create(init.agentDir);
  const keybindingsConfig = keybindings.getEffectiveConfig();

  // Real local resource discovery (same cwd/agentDir as the host) provides
  // prompt/skill command provenance the reduced wire CommandInfo cannot carry.
  const resourceLoader = new DefaultResourceLoader({
    cwd: init.cwd,
    agentDir: init.agentDir,
    eventBus,
    noExtensions: true,
    noThemes: true,
    noContextFiles: true,
  });
  try {
    await resourceLoader.reload();
  } catch {
    // Prompt/skill enrichment is best-effort; command names still flow.
  }

  const providers = new Map<string, RegisteredProvider>();

  let runtime: SidecarRuntime | undefined;
  let runner = new ExtensionRunner(
    loadResult.extensions,
    loadResult.runtime,
    init.cwd,
    session.sessionManager,
    modelRegistry,
  );

  for (const [name, value] of Object.entries(init.flagValues)) {
    runner.setFlagValue(name, value);
  }

  const commandCatalog = (): SlashCommandInfo[] => {
    const enrichment = new Map<string, SlashCommandInfo>();
    for (const prompt of resourceLoader.getPrompts().prompts) {
      enrichment.set(prompt.name, {
        name: prompt.name,
        description: prompt.description,
        source: "prompt",
        sourceInfo: prompt.sourceInfo,
      });
    }
    for (const skill of resourceLoader.getSkills().skills) {
      enrichment.set(skill.name, {
        name: skill.name,
        description: skill.description,
        source: "skill",
        sourceInfo: skill.sourceInfo,
      });
    }
    for (const command of runner.getRegisteredCommands()) {
      enrichment.set(command.invocationName, {
        name: command.invocationName,
        ...(command.description !== undefined ? { description: command.description } : {}),
        source: "extension",
        sourceInfo: command.sourceInfo,
      });
    }
    return state.current.commands.map((info) => {
      const local = enrichment.get(info.name);
      if (local !== undefined) {
        return info.description !== undefined ? { ...local, description: info.description } : local;
      }
      // Host-only command unknown to local discovery (e.g. built-in).
      return {
        name: info.name,
        ...(info.description !== undefined ? { description: info.description } : {}),
        source: "prompt",
        sourceInfo: createSyntheticSourceInfo(info.name, { source: "host" }),
      };
    });
  };

  const bridged = createBridgedActions({
    peer,
    state,
    session,
    cwd: init.cwd,
    getCommands: commandCatalog,
    createCommandContext: () => runner.createCommandContext(),
    getRegistrations: () => toWire(registrations()),
  });
  bridged.turnSignal.setIdle(init.state.idle);

  const providerActions = {
    registerProvider: (name: string, config: ProviderConfig) => {
      providers.set(name, { config });
      modelRegistry.registerProvider(name, config);
      peer.notify("provider/register", {
        name,
        configDto: sanitizeProviderConfig(config),
        ...(config.streamSimple !== undefined ? { hasStreamSimple: true } : {}),
      });
    },
    unregisterProvider: (name: string) => {
      providers.delete(name);
      modelRegistry.unregisterProvider(name);
      peer.notify("provider/unregister", { name });
    },
  };

  // Bound via wireRunner() once the runtime object exists (below).

  const registrations = (): RegistrationsDto => ({
    tools: runner.getAllRegisteredTools().map((tool) => toolRegistration(tool)),
    commands: runner.getRegisteredCommands().map((command) => ({
      name: command.invocationName,
      ...(command.description !== undefined ? { description: command.description } : {}),
      sourceInfo: command.sourceInfo,
      hasArgumentCompletions: command.getArgumentCompletions !== undefined,
    })),
    shortcuts: [...runner.getShortcuts(keybindingsConfig).entries()].map(([keyId, shortcut]) => ({
      keyId,
      ...(shortcut.description !== undefined ? { description: shortcut.description } : {}),
      extensionPath: shortcut.extensionPath,
    })),
    flags: [...runner.getFlags().values()].map((flag) => ({
      name: flag.name,
      ...(flag.description !== undefined ? { description: flag.description } : {}),
      type: flag.type,
      ...(flag.default !== undefined ? { default: flag.default } : {}),
      extensionPath: flag.extensionPath,
    })),
    providers: [...providers.entries()].map(([name, provider]) => ({
      name,
      configDto: sanitizeProviderConfig(provider.config),
      ...(provider.config.streamSimple !== undefined ? { hasStreamSimple: true } : {}),
      ...(provider.extensionPath !== undefined ? { extensionPath: provider.extensionPath } : {}),
    })),
  });

  const subscribedEvents = (): string[] => {
    const kinds = new Set<string>();
    for (const extension of loadResult.extensions) {
      for (const eventType of extension.handlers.keys()) kinds.add(eventType);
    }
    return [...kinds].sort();
  };

  const reportHandlerError = (error: ExtensionError): void => {
    peer.notify("error/extension", {
      extensionPath: error.extensionPath,
      event: error.event,
      error: error.error,
      ...(error.stack !== undefined ? { stack: error.stack } : {}),
    });
  };

  // Wiring shared by the boot runner and every reload replacement: action
  const wireRunner = (target: ExtensionRunner): void => {
    // Error listener FIRST: bindCore flushes queued provider registrations
    // and reports their failures through emitError.
    target.onError(reportHandlerError);
    target.bindCore(bridged.actions, bridged.contextActions, providerActions);
    target.bindCommandContext(bridged.commandContextActions);
    if (runtime !== undefined && options.uiContext !== undefined) {
      target.setUIContext(options.uiContext(runtime), init.mode);
    } else {
      target.setUIContext(undefined, init.mode);
    }
  };

  const loadMore = async (paths: string[]) => {
    // Load ONLY genuinely new paths: factories of already-loaded extensions
    // must not re-run (their load-time side effects are not idempotent).
    const known = new Set<string>();
    for (const extension of loadResult.extensions) {
      known.add(extension.path);
      known.add(extension.resolvedPath);
    }
    const newPaths = paths.filter((path) => !known.has(path));
    const extra = await loadExtensions(newPaths, init.cwd, eventBus, loadResult.runtime);
    const knownResolved = new Set(loadResult.extensions.map((extension: Extension) => extension.resolvedPath));
    for (const extension of extra.extensions) {
      if (!knownResolved.has(extension.resolvedPath)) loadResult.extensions.push(extension);
    }
    const replacement = new ExtensionRunner(
      loadResult.extensions,
      loadResult.runtime,
      init.cwd,
      session.sessionManager,
      modelRegistry,
    );
    // NOTE: no runner.invalidate() here — the shared ExtensionRuntime stays
    // live; invalidating it would permanently poison every extension API.
    runner = replacement;
    if (runtime !== undefined) runtime.runner = replacement;
    wireRunner(replacement);
    return {
      registrations: registrations(),
      errors: extra.errors.map((entry) => ({ extensionPath: entry.path, event: "load", error: entry.error })),
    };
  };

  runtime = {
    runner,
    loadResult,
    bridged,
    state,
    session,
    peer,
    providers,
    keybindingsConfig,
    keybindings,
    eventBus,
    mode: init.mode,
    hasUi: init.hasUi,
    cwd: init.cwd,
    hostThemeCatalog: options.hostThemeCatalog,
    loadErrors: loadResult.errors.map((entry) => ({
      extensionPath: entry.path,
      event: "load",
      error: entry.error,
    })),
    loadMore,
    registrations,
    subscribedEvents,
  };

  wireRunner(runner);

  // Session setup callbacks (newSession/fork setup + withSession).
  peer.onRequest("session/setup", async (params) => {
    const token = typeof params === "object" && params !== null && !Array.isArray(params) ? params["token"] : undefined;
    const pending = typeof token === "string" ? bridged.pendingSetups.get(token) : undefined;
    if (pending === undefined || typeof token !== "string") {
      throw new Error(`unknown session setup token: ${String(token)}`);
    }
    bridged.pendingSetups.delete(token);
    if (pending.kind === "setup") {
      const setupSession = session.createSetupSession();
      await pending.run(setupSession);
      return {
        entries: toWire(setupSession.getEntries()),
        name: setupSession.getSessionName() ?? null,
      };
    }
    await pending.run(createReplacedSessionContext(runner.createCommandContext(), peer));
    return { entries: [] };
  });

  return runtime;
}

function toolRegistration(tool: RegisteredTool) {
  const definition = tool.definition;
  return {
    name: definition.name,
    label: definition.label,
    description: definition.description,
    parameters: JSON.parse(JSON.stringify(definition.parameters)) as JsonValue,
    ...(definition.promptSnippet !== undefined ? { promptSnippet: definition.promptSnippet } : {}),
    ...(definition.promptGuidelines !== undefined
      ? { promptGuidelines: definition.promptGuidelines.join("\n") }
      : {}),
    sourceInfo: tool.sourceInfo,
    hasRenderCall: definition.renderCall !== undefined,
    hasRenderResult: definition.renderResult !== undefined,
  };
}

/** Serializable projection of a ProviderConfig (functions dropped). */
export function sanitizeProviderConfig(config: ProviderConfig): JsonValue {
  return JSON.parse(
    JSON.stringify(config, (_key, value: unknown) => (typeof value === "function" ? undefined : value)),
  ) as JsonValue;
}
