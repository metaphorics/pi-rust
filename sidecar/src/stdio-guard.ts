/**
 * Stdio guard — invariant I2 of the bridge: the protocol channel is written
 * ONLY through the original `process.stdout.write` captured before any
 * extension (or pi runtime) code runs. Everything extensions print — via
 * `console.*`, `process.stdout.write`, or `process.stderr.write` — lands in a
 * virtual sink and never corrupts the NDJSON stream.
 *
 * `installStdioGuard()` MUST be the first statement of the entry point.
 */

import { inspect } from "node:util";
export interface StdioSink {
  /** Captured chunks in arrival order. */
  readonly chunks: Array<{ stream: "stdout" | "stderr"; data: string }>;
  onWrite?: (stream: "stdout" | "stderr", data: string) => void;
}

type WriteFn = typeof process.stdout.write;

let originalStdoutWrite: WriteFn | undefined;
let sink: StdioSink | undefined;

/**
 * Capture the real stdout writer and reroute stdout/stderr into the sink.
 * Idempotent; the first call wins.
 */
export function installStdioGuard(): StdioSink {
  if (sink !== undefined) return sink;
  originalStdoutWrite = process.stdout.write.bind(process.stdout);
  const captured: StdioSink = { chunks: [] };
  sink = captured;

  const redirect = (stream: "stdout" | "stderr"): WriteFn => {
    return (
      chunk: string | Uint8Array,
      encodingOrCallback?: BufferEncoding | ((error?: Error | null) => void),
      callback?: (error?: Error | null) => void,
    ): boolean => {
      const data = typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk);
      captured.chunks.push({ stream, data });
      captured.onWrite?.(stream, data);
      const cb = typeof encodingOrCallback === "function" ? encodingOrCallback : callback;
      cb?.(null);
      return true;
    };
  };

  process.stdout.write = redirect("stdout") as typeof process.stdout.write;
  process.stderr.write = redirect("stderr") as typeof process.stderr.write;

  // Under Bun, console.* writes straight to fd 1/2 and NEVER goes through
  // process.stdout.write — patch the console methods themselves.
  const consoleCapture = (stream: "stdout" | "stderr") => {
    return (...args: unknown[]): void => {
      const data = `${args.map((arg) => (typeof arg === "string" ? arg : inspect(arg))).join(" ")}\n`;
      captured.chunks.push({ stream, data });
      captured.onWrite?.(stream, data);
    };
  };
  console.log = consoleCapture("stdout");
  console.info = consoleCapture("stdout");
  console.debug = consoleCapture("stdout");
  console.trace = consoleCapture("stderr");
  console.warn = consoleCapture("stderr");
  console.error = consoleCapture("stderr");

  return captured;
}

/** Write protocol bytes through the pre-guard stdout writer. */
export function protocolWrite(bytes: Uint8Array): void {
  if (originalStdoutWrite === undefined) {
    throw new Error("stdio guard is not installed; protocol channel unavailable");
  }
  originalStdoutWrite(bytes);
}

/** The active sink, if the guard is installed. */
export function stdioSink(): StdioSink | undefined {
  return sink;
}
