/**
 * PIN NOTE (0.80.7): access to pi modules that are real published dist files
 * but not re-exported from the package root, whose `exports` map only
 * exposes "." and "./rpc-entry".
 *
 * The brief's fixed strategy is to REUSE pi's runtime, not reimplement it,
 * so these deep imports go through relative node_modules paths (Bun and tsc
 * both resolve them, bypassing the exports map). Module identity is shared
 * with the root entry — verified: `ExtensionRunner` from the root import is
 * the same object as from the deep path. The exact-version pin plus the
 * compat suite gate any version bump.
 *
 * Everything root-exported is imported from the package root elsewhere; ONLY
 * genuinely unreachable members live here.
 */

export {
  emitProjectTrustEvent,
  emitSessionShutdownEvent,
} from "../node_modules/@earendil-works/pi-coding-agent/dist/core/extensions/runner.js";
export { loadExtensions } from "../node_modules/@earendil-works/pi-coding-agent/dist/core/extensions/loader.js";
export { KeybindingsManager } from "../node_modules/@earendil-works/pi-coding-agent/dist/core/keybindings.js";
export type { KeybindingsConfig } from "../node_modules/@earendil-works/pi-coding-agent/dist/core/keybindings.js";
export {
  getAvailableThemesWithPaths,
  getThemeByName,
  loadThemeFromPath,
  setThemeInstance,
  theme as activeTheme,
} from "../node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/theme/theme.js";
export type {
  MessageEndEvent,
  ReplacedSessionContext,
} from "../node_modules/@earendil-works/pi-coding-agent/dist/core/extensions/types.js";
