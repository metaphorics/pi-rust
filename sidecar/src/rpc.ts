/**
 * NDJSON RPC peer over a byte transport.
 *
 * Symmetric full-duplex: each side allocates ids from its own space, so the
 * correlation key is (direction, id). This peer keeps two tables:
 * - `pending`: our outbound requests awaiting the peer's response;
 * - `inbound`: the peer's requests we are currently serving (each holds an
 *   AbortController minted for that request; a `cancel` frame aborts it).
 *
 * Framing, validation, and limits live in `protocol.ts`; this module owns
 * correlation, cancellation, and dispatch only.
 */

import {
  type Envelope,
  FrameError,
  type JsonValue,
  MAX_FRAME_BYTES,
  type NotificationMethod,
  type ProtocolError,
  type RequestMethod,
  decodeFrame,
  encodeFrame,
} from "./protocol.ts";

/** A rejected outbound request carries the peer's structured error. */
export class RpcError extends Error {
  constructor(readonly detail: ProtocolError) {
    super(detail.message);
    this.name = "RpcError";
  }
}

/** Outbound request cancelled through its AbortSignal. */
export class RpcCancelledError extends Error {
  constructor(readonly method: string) {
    super(`request ${method} was cancelled`);
    this.name = "RpcCancelledError";
  }
}

export type RequestHandler = (params: JsonValue, signal: AbortSignal) => Promise<unknown> | unknown;
export type NotificationHandler = (params: JsonValue) => void;

interface PendingRequest {
  method: string;
  resolve: (value: JsonValue) => void;
  reject: (error: Error) => void;
  cleanup?: () => void;
}

export interface RpcPeerOptions {
  write: (bytes: Uint8Array) => void;
  /** Diagnostic sink for skipped/bad frames. Never the protocol channel. */
  onTransportError?: (error: Error, line?: string) => void;
}

export class RpcPeer {
  private readonly write: (bytes: Uint8Array) => void;
  private readonly onTransportError: (error: Error, line?: string) => void;
  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private readonly inbound = new Map<number, AbortController>();
  private readonly requestHandlers = new Map<string, RequestHandler>();
  private readonly notificationHandlers = new Map<string, NotificationHandler>();
  private buffer: Uint8Array = new Uint8Array(0);
  private closed = false;

  constructor(options: RpcPeerOptions) {
    this.write = options.write;
    this.onTransportError = options.onTransportError ?? (() => {});
  }

  onRequest(method: RequestMethod, handler: RequestHandler): void {
    this.requestHandlers.set(method, handler);
  }

  onNotification(method: NotificationMethod, handler: NotificationHandler): void {
    this.notificationHandlers.set(method, handler);
  }

  /** Send a request and await the peer's response. */
  request(method: RequestMethod, params: JsonValue, options?: { signal?: AbortSignal }): Promise<JsonValue> {
    const id = this.nextId++;
    const { promise, resolve, reject } = Promise.withResolvers<JsonValue>();
    const entry: PendingRequest = { method, resolve, reject };
    const signal = options?.signal;
    if (signal !== undefined) {
      if (signal.aborted) {
        reject(new RpcCancelledError(method));
        return promise;
      }
      const onAbort = () => {
        if (!this.pending.delete(id)) return;
        this.send({ type: "cancel", id });
        reject(new RpcCancelledError(method));
      };
      signal.addEventListener("abort", onAbort, { once: true });
      entry.cleanup = () => signal.removeEventListener("abort", onAbort);
    }
    this.pending.set(id, entry);
    try {
      this.send({ type: "req", id, method, params });
    } catch (error) {
      this.pending.delete(id);
      entry.cleanup?.();
      reject(error instanceof Error ? error : new Error(String(error)));
    }
    return promise;
  }

  /** Send a fire-and-forget notification. */
  notify(method: NotificationMethod, params: JsonValue): void {
    this.send({ type: "ev", method, params });
  }

  /** Feed raw transport bytes; complete NDJSON lines are dispatched. */
  feed(chunk: Uint8Array): void {
    const combined = new Uint8Array(this.buffer.length + chunk.length);
    combined.set(this.buffer, 0);
    combined.set(chunk, this.buffer.length);
    let start = 0;
    for (let i = 0; i < combined.length; i++) {
      if (combined[i] !== 0x0a) continue;
      const line = combined.subarray(start, i + 1);
      start = i + 1;
      this.dispatchLine(line);
    }
    this.buffer = combined.subarray(start);
    if (this.buffer.length > MAX_FRAME_BYTES) {
      this.buffer = new Uint8Array(0);
      this.onTransportError(new FrameError("oversize", "unterminated frame exceeded the frame size bound"));
    }
  }

  /** Reject every in-flight outbound request (peer went away). */
  fail(error: Error): void {
    this.closed = true;
    const entries = [...this.pending.values()];
    this.pending.clear();
    for (const entry of entries) {
      entry.cleanup?.();
      entry.reject(error);
    }
  }

  private send(envelope: Envelope): void {
    if (this.closed) {
      throw new Error("rpc peer is closed");
    }
    this.write(encodeFrame(envelope));
  }

  private dispatchLine(line: Uint8Array): void {
    let envelope: Envelope;
    try {
      envelope = decodeFrame(line);
    } catch (error) {
      if (error instanceof FrameError && error.code === "empty") return;
      // One bad line never kills the channel: log and skip.
      this.onTransportError(
        error instanceof Error ? error : new Error(String(error)),
        new TextDecoder().decode(line),
      );
      return;
    }
    switch (envelope.type) {
      case "req":
        this.dispatchRequest(envelope.id, envelope.method, envelope.params);
        break;
      case "res": {
        const pending = this.pending.get(envelope.id);
        if (pending === undefined) return; // Cancelled or unknown: drop.
        this.pending.delete(envelope.id);
        pending.cleanup?.();
        if ("ok" in envelope) {
          pending.resolve(envelope.ok);
        } else {
          pending.reject(new RpcError(envelope.err));
        }
        break;
      }
      case "ev": {
        const handler = this.notificationHandlers.get(envelope.method);
        if (handler === undefined) {
          this.onTransportError(new Error(`no handler for notification ${envelope.method}`));
          return;
        }
        try {
          handler(envelope.params);
        } catch (error) {
          this.onTransportError(error instanceof Error ? error : new Error(String(error)));
        }
        break;
      }
      case "cancel":
        this.inbound.get(envelope.id)?.abort();
        break;
    }
  }

  private dispatchRequest(id: number, method: string, params: JsonValue): void {
    const handler = this.requestHandlers.get(method);
    if (handler === undefined) {
      this.respondErr(id, { code: "unknown_method", message: `no handler for request ${method}` });
      return;
    }
    if (this.inbound.has(id)) {
      // Id reuse while in flight is a peer protocol violation. There is no
      // correlatable way to answer the duplicate (any response would settle
      // the peer's original pending request), so report and drop it; the
      // original request keeps its controller and answers normally.
      this.onTransportError(new Error(`request id ${id} is already in flight; dropping duplicate ${method}`));
      return;
    }
    const controller = new AbortController();
    this.inbound.set(id, controller);
    void (async () => {
      try {
        const result = await handler(params, controller.signal);
        if (!this.inbound.delete(id)) return;
        this.send({ type: "res", id, ok: (result ?? null) as JsonValue });
      } catch (error) {
        if (!this.inbound.delete(id)) return;
        this.respondErr(id, toProtocolError(error));
      }
    })();
  }

  private respondErr(id: number, err: ProtocolError): void {
    try {
      this.send({ type: "res", id, err });
    } catch (sendError) {
      this.onTransportError(sendError instanceof Error ? sendError : new Error(String(sendError)));
    }
  }
}

/** Map a thrown value onto the wire error DTO, clamping oversized texts. */
export function toProtocolError(error: unknown): ProtocolError {
  if (error instanceof RpcError) return error.detail;
  const message = error instanceof Error ? error.message : String(error);
  const detail: ProtocolError = {
    code: error instanceof Error && error.name !== "Error" ? error.name : "handler_error",
    message: message.slice(0, 64 * 1024),
  };
  if (error instanceof Error && typeof error.stack === "string") {
    detail.stack = error.stack.slice(0, 512 * 1024);
  }
  return detail;
}
