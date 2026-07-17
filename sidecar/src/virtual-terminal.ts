/**
 * Virtual terminal — implements pi-tui's `Terminal` interface with no real
 * TTY. The headless npm pi-tui instance (frames.ts) runs against it; every
 * write is captured, and dimensions are fed from the Rust host. Stray
 * escape-sequence traffic from components lands here, never on stdio.
 */

import type { Terminal } from "@earendil-works/pi-tui";

export class VirtualTerminal implements Terminal {
  private _columns = 80;
  private _rows = 24;
  private inputHandler: ((data: string) => void) | undefined;
  private resizeHandler: (() => void) | undefined;
  private started = false;
  /** Captured writes, in order (assertable in tests). */
  readonly writes: string[] = [];
  title = "";
  progressActive = false;
  cursorHidden = false;

  start(onInput: (data: string) => void, onResize: () => void): void {
    this.started = true;
    this.inputHandler = onInput;
    this.resizeHandler = onResize;
  }

  stop(): void {
    this.started = false;
    this.inputHandler = undefined;
    this.resizeHandler = undefined;
  }

  isStarted(): boolean {
    return this.started;
  }

  async drainInput(): Promise<void> {
    // Nothing buffers here; input arrives only via feedInput().
  }

  write(data: string): void {
    this.writes.push(data);
  }

  get columns(): number {
    return this._columns;
  }

  get rows(): number {
    return this._rows;
  }

  get kittyProtocolActive(): boolean {
    return false;
  }

  moveBy(_lines: number): void {}

  hideCursor(): void {
    this.cursorHidden = true;
  }

  showCursor(): void {
    this.cursorHidden = false;
  }

  clearLine(): void {}

  clearFromCursor(): void {}

  clearScreen(): void {
    this.writes.length = 0;
  }

  setTitle(title: string): void {
    this.title = title;
  }

  setProgress(active: boolean): void {
    this.progressActive = active;
  }

  /** Host-driven dimension updates (`lifecycle/init` and `ui/resize`). */
  resize(columns: number, rows?: number): void {
    if (columns === this._columns && (rows === undefined || rows === this._rows)) return;
    this._columns = columns;
    if (rows !== undefined) this._rows = rows;
    this.resizeHandler?.();
  }

  /** Feed key input to whatever the headless TUI has focused. */
  feedInput(data: string): void {
    this.inputHandler?.(data);
  }
}
