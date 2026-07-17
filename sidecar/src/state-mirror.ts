/**
 * Host-state cache backing the SYNC getters of the extension API surface.
 *
 * pi resolves these values "at call time" (runner.ts createContext); across
 * the process boundary the same freshness is provided at event-dispatch
 * granularity: every `event/emit`/`event/notify` carries a fresh state block,
 * and `state/update` notifications patch the cache between events.
 *
 * SYNC-VOID actions apply their deterministic local effect here immediately
 * (optimistic apply) so read-after-write inside one handler observes the
 * write, matching in-process pi.
 */

import type { JsonObject, JsonValue, StateBlockDto, StateUpdateDto, ThemeDto } from "./protocol.ts";

export class StateMirror {
  private state: StateBlockDto;
  /** Fires after any state change (used to refresh themed frames). */
  onThemeChange?: (theme: ThemeDto) => void;
  onEditorTextChange?: (text: string) => void;

  constructor(initial: StateBlockDto) {
    this.state = initial;
  }

  /** Replace the whole block (piggybacked on every event dispatch). */
  apply(block: StateBlockDto): void {
    const themeChanged = JSON.stringify(block.theme) !== JSON.stringify(this.state.theme);
    const editorChanged = block.editorText !== this.state.editorText;
    this.state = block;
    if (themeChanged) this.onThemeChange?.(block.theme);
    if (editorChanged) this.onEditorTextChange?.(block.editorText);
  }
  /** Patch from a `state/update` notification. */
  applyUpdate(update: StateUpdateDto): void {
    if (update.model !== undefined) this.state.model = update.model;
    if (update.idle !== undefined) this.state.idle = update.idle;
    if (update.thinkingLevel !== undefined) this.state.thinkingLevel = update.thinkingLevel;
    if (update.activeTools !== undefined) this.state.activeTools = update.activeTools;
    if (update.allTools !== undefined) this.state.allTools = update.allTools;
    if (update.contextUsage !== undefined) this.state.contextUsage = update.contextUsage;
    if (update.systemPrompt !== undefined) this.state.systemPrompt = update.systemPrompt;
    if (update.footer !== undefined) this.state.footer = update.footer;
    if (update.editorText !== undefined) {
      this.state.editorText = update.editorText;
      this.onEditorTextChange?.(update.editorText);
    }
    if (update.toolsExpanded !== undefined) this.state.toolsExpanded = update.toolsExpanded;
    if (update.theme !== undefined) {
      this.state.theme = update.theme;
      this.onThemeChange?.(update.theme);
    }
  }

  get current(): StateBlockDto {
    return this.state;
  }

  get model(): JsonObject | undefined {
    return this.state.model;
  }

  get idle(): boolean {
    return this.state.idle;
  }

  get theme(): ThemeDto {
    return this.state.theme;
  }

  get footer(): JsonValue | undefined {
    return this.state.footer;
  }

  // ----- optimistic applies for SYNC-VOID actions -----

  setThinkingLevel(level: string): void {
    this.state.thinkingLevel = level;
  }

  setActiveTools(toolNames: string[]): void {
    this.state.activeTools = toolNames;
  }

  setSessionName(name: string): void {
    this.state.sessionName = name;
  }

  setEditorText(text: string): void {
    this.state.editorText = text;
    this.onEditorTextChange?.(text);
  }

  setToolsExpanded(expanded: boolean): void {
    this.state.toolsExpanded = expanded;
  }
}
