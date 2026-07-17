/**
 * Golden-fixture lock against `crates/pi-ext-protocol` (single source of truth).
 *
 * The fixtures are consumed directly from the Rust crate so both sides always
 * test the same bytes. Every fixture must decode, re-encode byte-identically,
 * and belong to the documented direction/family matrix. Sidecar-to-rust
 * fixtures are additionally reproduced from envelopes CONSTRUCTED in TS,
 * proving the production encoder (not just the decoder round-trip) emits the
 * canonical bytes.
 */

import { describe, expect, test } from "bun:test";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

import {
  type Envelope,
  FrameError,
  MAX_ERROR_MESSAGE_BYTES,
  MAX_FRAME_BYTES,
  PROTOCOL_VERSION,
  decodeFrame,
  encodeFrame,
} from "../src/protocol.ts";
import { RpcCancelledError, RpcError, RpcPeer } from "../src/rpc.ts";

const FIXTURE_DIR = join(import.meta.dir, "..", "..", "crates", "pi-ext-protocol", "fixtures");

/** Mirrors golden.rs `get_method_direction`. */
const METHOD_DIRECTIONS: Record<string, "rust-to-sidecar" | "sidecar-to-rust"> = {
  "lifecycle/init": "rust-to-sidecar",
  "event/emit": "rust-to-sidecar",
  "ui/terminal_input": "rust-to-sidecar",
  "ui/focus": "rust-to-sidecar",
  "ui/resize": "rust-to-sidecar",
  "tool/execute": "rust-to-sidecar",
  "provider/stream": "rust-to-sidecar",
  "command/execute": "rust-to-sidecar",
  "shortcut/invoke": "rust-to-sidecar",
  "session/setup": "rust-to-sidecar",
  "session/sync": "rust-to-sidecar",
  "state/update": "rust-to-sidecar",
  "lifecycle/initialized": "sidecar-to-rust",
  "action/sendUserMessage": "sidecar-to-rust",
  "ui/getTheme": "sidecar-to-rust",
  "ui/getAllThemes": "sidecar-to-rust",
  "ui/editorSubmit": "sidecar-to-rust",
  "ui/editorChange": "sidecar-to-rust",
  "ui/terminalInputActive": "sidecar-to-rust",
  "tool/update": "sidecar-to-rust",
  "provider/register": "sidecar-to-rust",
  "error/extension": "sidecar-to-rust",
};

const RESULT_FIXTURES = [
  "sidecar-to-rust-ui-terminal-input-result.json",
  "rust-to-sidecar-ui-theme-catalog-result.json",
  "rust-to-sidecar-ui-theme-lookup-result.json",
  "sidecar-to-rust-tool-execute-result.json",
];

const fixtureNames = readdirSync(FIXTURE_DIR).sort();

function fixtureBytes(name: string): Uint8Array {
  return new Uint8Array(readFileSync(join(FIXTURE_DIR, name)));
}

describe("golden fixtures", () => {
  test("crate fixture directory is reachable and complete", () => {
    expect(fixtureNames.length).toBe(26);
  });

  for (const name of fixtureNames) {
    test(`${name} decodes and re-encodes byte-exactly`, () => {
      const bytes = fixtureBytes(name);
      expect(bytes[bytes.length - 1]).toBe(0x0a);
      const decoded = decodeFrame(bytes);
      const encoded = encodeFrame(decoded);
      expect(Buffer.from(encoded).toString("latin1")).toBe(Buffer.from(bytes).toString("latin1"));
    });
  }

  test("method fixtures cover the full direction/family matrix", () => {
    const covered = new Set<string>();
    for (const name of fixtureNames) {
      if (RESULT_FIXTURES.includes(name)) continue;
      const decoded = decodeFrame(fixtureBytes(name));
      if (decoded.type !== "req" && decoded.type !== "ev") {
        throw new Error(`${name}: method fixture must be a request or event`);
      }
      const direction = METHOD_DIRECTIONS[decoded.method];
      expect(direction).toBeDefined();
      expect(name.startsWith(direction ?? "")).toBe(true);
      const family = decoded.method.split("/")[0] ?? "";
      covered.add(`${direction}:${family}`);
    }
    expect([...covered].sort()).toEqual(
      [
        "rust-to-sidecar:lifecycle",
        "rust-to-sidecar:event",
        "rust-to-sidecar:ui",
        "rust-to-sidecar:tool",
        "rust-to-sidecar:provider",
        "rust-to-sidecar:command",
        "rust-to-sidecar:shortcut",
        "rust-to-sidecar:session",
        "rust-to-sidecar:state",
        "sidecar-to-rust:lifecycle",
        "sidecar-to-rust:action",
        "sidecar-to-rust:ui",
        "sidecar-to-rust:tool",
        "sidecar-to-rust:provider",
        "sidecar-to-rust:error",
      ].sort(),
    );
  });
});

describe("constructed sidecar-to-rust envelopes reproduce fixture bytes", () => {
  const cases: Array<{ fixture: string; envelope: Envelope }> = [
    {
      fixture: "sidecar-to-rust-lifecycle.json",
      envelope: {
        type: "ev",
        method: "lifecycle/initialized",
        params: {
          registrations: { tools: [], commands: [], shortcuts: [], flags: [], providers: [] },
          subscribedEvents: ["input", "tool_call"],
          errors: [],
        },
      },
    },
    {
      fixture: "sidecar-to-rust-action.json",
      envelope: {
        type: "ev",
        method: "action/sendUserMessage",
        params: {
          content: {
            text: "follow this",
            images: [{ type: "image", mimeType: "image/png", data: "AA==" }],
          },
          deliverAs: "followUp",
        },
      },
    },
    {
      fixture: "sidecar-to-rust-error.json",
      envelope: {
        type: "ev",
        method: "error/extension",
        params: {
          extensionPath: "/extensions/failing.ts",
          event: "before_agent_start",
          error: "fixture failure",
          stack: "Error: fixture failure\n    at failing.ts:1:1",
        },
      },
    },
    {
      fixture: "sidecar-to-rust-provider.json",
      envelope: {
        type: "ev",
        method: "provider/register",
        params: {
          name: "fixture",
          configDto: {
            api: "fixture-api",
            baseUrl: "https://example.test",
            models: [{ id: "fixture-model", name: "Fixture Model" }],
          },
          extensionPath: "/extensions/provider.ts",
        },
      },
    },
    {
      fixture: "sidecar-to-rust-tool.json",
      envelope: {
        type: "ev",
        method: "tool/update",
        params: {
          toolCallId: "call-1",
          partial: { content: [{ type: "text", text: "working" }], details: { progress: 0.5 } },
        },
      },
    },
    {
      fixture: "sidecar-to-rust-ui.json",
      envelope: { type: "req", id: 4, method: "ui/getTheme", params: { name: "dark" } },
    },
    {
      fixture: "sidecar-to-rust-ui-get-all-themes.json",
      envelope: { type: "req", id: 5, method: "ui/getAllThemes", params: {} },
    },
    {
      fixture: "sidecar-to-rust-ui-terminal-input-result.json",
      envelope: { type: "res", id: 3, ok: { consume: false, data: "rewritten" } },
    },
    {
      fixture: "sidecar-to-rust-tool-execute-result.json",
      envelope: {
        type: "res",
        id: 6,
        ok: {
          content: [{ type: "text", text: "saved" }],
          details: { progress: 1 },
          isError: false,
          addedToolNames: ["new_tool"],
          terminate: true,
        },
      },
    },
  ];

  for (const { fixture, envelope } of cases) {
    test(fixture, () => {
      const expected = fixtureBytes(fixture);
      const encoded = encodeFrame(envelope);
      expect(Buffer.from(encoded).toString("latin1")).toBe(Buffer.from(expected).toString("latin1"));
    });
  }
});

describe("frame limits and validation", () => {
  test("empty frames are rejected", () => {
    expect(() => decodeFrame(new Uint8Array(0))).toThrow(FrameError);
    expect(() => decodeFrame(new TextEncoder().encode("\n"))).toThrow(FrameError);
  });

  test("multi-line frames are rejected", () => {
    const bytes = new TextEncoder().encode('{"type":"cancel",\n"id":1}\n');
    expect(() => decodeFrame(bytes)).toThrow("more than one JSON line");
  });

  test("oversize frames are rejected on both encode and decode", () => {
    const bigParams = { text: "x".repeat(MAX_FRAME_BYTES) };
    expect(() => encodeFrame({ type: "ev", method: "ui/setTitle", params: bigParams })).toThrow("maximum");
    const oversized = new Uint8Array(MAX_FRAME_BYTES + 1).fill(0x20);
    expect(() => decodeFrame(oversized)).toThrow("maximum");
  });

  test("request ids must be positive integers (NonZeroU64 mirror)", () => {
    const zero = new TextEncoder().encode('{"type":"cancel","id":0}\n');
    expect(() => decodeFrame(zero)).toThrow("positive integer");
    expect(() => decodeFrame(new TextEncoder().encode('{"type":"cancel","id":1.5}\n'))).toThrow(
      "positive integer",
    );
  });

  test("unknown methods are rejected per frame kind", () => {
    // "ui/frame" is a notification method, not a request method.
    const req = new TextEncoder().encode('{"type":"req","id":1,"method":"ui/frame","params":{}}\n');
    expect(() => decodeFrame(req)).toThrow("unknown request method");
    const ev = new TextEncoder().encode('{"type":"ev","method":"tool/execute","params":{}}\n');
    expect(() => decodeFrame(ev)).toThrow("unknown notification method");
  });

  test("ui/input is valid in both frame kinds (dialog request vs component input)", () => {
    const req = new TextEncoder().encode('{"type":"req","id":7,"method":"ui/input","params":{"title":"t"}}\n');
    expect(decodeFrame(req).type).toBe("req");
    const ev = new TextEncoder().encode('{"type":"ev","method":"ui/input","params":{"slot":"editor","data":"x"}}\n');
    expect(decodeFrame(ev).type).toBe("ev");
  });

  test("hello with a mismatched protocol version is invalid", () => {
    const hello = { type: "ev", method: "lifecycle/hello", params: { protocol: PROTOCOL_VERSION + 1, pi: "0.80.7", bun: "1.0.0" } } as const;
    expect(() => encodeFrame(hello as Envelope)).toThrow("unsupported extension protocol version");
  });

  test("extension error texts are bounded", () => {
    const params = {
      extensionPath: "/e.ts",
      event: "input",
      error: "y".repeat(MAX_ERROR_MESSAGE_BYTES + 1),
    };
    expect(() => encodeFrame({ type: "ev", method: "error/extension", params })).toThrow(
      "exceeds its size bound",
    );
  });
});

describe("rpc peer correlation", () => {
  // Direct wiring: each peer's writes feed the other synchronously, so tests
  // are driven purely by microtask settlement — no wall-clock timers.
  function pair(): { a: RpcPeer; b: RpcPeer } {
    let b: RpcPeer;
    const a = new RpcPeer({ write: (bytes) => b.feed(bytes) });
    b = new RpcPeer({ write: (bytes) => a.feed(bytes) });
    return { a, b };
  }

  // Async request handlers settle over a bounded number of microtask hops.
  async function flushMicrotasks(): Promise<void> {
    for (let i = 0; i < 16; i++) await Promise.resolve();
  }

  test("request/response round-trips with per-direction id spaces", async () => {
    const { a, b } = pair();
    b.onRequest("ui/getTheme", (params) => ({ echoed: params }));
    // Both sides allocate id 1 independently; correlation stays direction-scoped.
    a.onRequest("ui/getAllThemes", () => [{ name: "dark" }]);
    const [fromA, fromB] = await Promise.all([
      a.request("ui/getTheme", { name: "dark" }),
      b.request("ui/getAllThemes", {}),
    ]);
    expect(fromA).toEqual({ echoed: { name: "dark" } });
    expect(fromB).toEqual([{ name: "dark" }]);
  });

  test("handler errors surface as structured RpcError", async () => {
    const { a, b } = pair();
    b.onRequest("tool/execute", () => {
      throw new Error("tool exploded");
    });
    await expect(a.request("tool/execute", { toolCallId: "c", name: "t", args: {} })).rejects.toThrow(RpcError);
  });

  test("unknown request methods respond with unknown_method", async () => {
    const { a } = pair();
    try {
      await a.request("command/execute", { name: "x", args: "" });
      throw new Error("should have rejected");
    } catch (error) {
      expect(error).toBeInstanceOf(RpcError);
      expect((error as RpcError).detail.code).toBe("unknown_method");
    }
  });

  test("aborting an outbound request sends cancel and aborts the inbound signal", async () => {
    const { a, b } = pair();
    let inboundAborted = false;
    const handled = Promise.withResolvers<void>();
    b.onRequest("ui/select", (_params, signal) => {
      const { promise, resolve } = Promise.withResolvers<string>();
      signal.addEventListener("abort", () => {
        inboundAborted = true;
        resolve("aborted");
        handled.resolve();
      });
      return promise;
    });
    const controller = new AbortController();
    const requestPromise = a.request("ui/select", { title: "t", options: ["a"] }, { signal: controller.signal });
    // Let the request reach the peer's handler before cancelling.
    await flushMicrotasks();
    controller.abort();
    await expect(requestPromise).rejects.toThrow(RpcCancelledError);
    await handled.promise;
    expect(inboundAborted).toBe(true);
  });

  test("notifications dispatch without correlation", async () => {
    const { a, b } = pair();
    const seen = Promise.withResolvers<unknown>();
    b.onNotification("state/update", (params) => seen.resolve(params));
    a.notify("state/update", { idle: true });
    expect(await seen.promise).toEqual({ idle: true });
  });

  test("a malformed line is skipped without killing the channel", async () => {
    const errors: Error[] = [];
    const out: Uint8Array[] = [];
    const peer = new RpcPeer({ write: (bytes) => out.push(bytes), onTransportError: (e) => errors.push(e) });
    peer.feed(new TextEncoder().encode("this is not json\n"));
    expect(errors.length).toBe(1);
    // Channel still works: an inbound request gets a response.
    peer.onRequest("ui/getAllThemes", () => []);
    peer.feed(new TextEncoder().encode('{"type":"req","id":9,"method":"ui/getAllThemes","params":{}}\n'));
    await flushMicrotasks();
    const response = out.map((bytes) => new TextDecoder().decode(bytes)).find((line) => line.includes('"id":9'));
    expect(response).toBe('{"type":"res","id":9,"ok":[]}\n');
  });

  test("split and batched chunks reassemble into frames", async () => {
    const out: Uint8Array[] = [];
    const peer = new RpcPeer({ write: (bytes) => out.push(bytes) });
    peer.onRequest("ui/getAllThemes", () => []);
    const frame = '{"type":"req","id":2,"method":"ui/getAllThemes","params":{}}\n';
    const second = '{"type":"req","id":3,"method":"ui/getAllThemes","params":{}}\n';
    const encoder = new TextEncoder();
    peer.feed(encoder.encode(frame.slice(0, 10)));
    peer.feed(encoder.encode(frame.slice(10) + second));
    await flushMicrotasks();
    expect(out.length).toBe(2);
  });

  test("a duplicate in-flight inbound id is dropped without clobbering the original", async () => {
    const out: string[] = [];
    const errors: Error[] = [];
    const peer = new RpcPeer({
      write: (bytes) => out.push(new TextDecoder().decode(bytes)),
      onTransportError: (e) => errors.push(e),
    });
    const gate = Promise.withResolvers<string>();
    let aborted = false;
    peer.onRequest("ui/select", (_params, signal) => {
      signal.addEventListener("abort", () => {
        aborted = true;
      });
      return gate.promise;
    });
    const encoder = new TextEncoder();
    const request = '{"type":"req","id":5,"method":"ui/select","params":{"title":"t","options":[]}}\n';
    peer.feed(encoder.encode(request));
    peer.feed(encoder.encode(request)); // same id while the first is still in flight
    await flushMicrotasks();
    // The duplicate is reported and produces no wire traffic at all.
    expect(errors.some((e) => e.message.includes("already in flight"))).toBe(true);
    expect(out.length).toBe(0);
    expect(aborted).toBe(false);
    // The original request is still live and correlates correctly.
    gate.resolve("done");
    await flushMicrotasks();
    expect(out).toEqual(['{"type":"res","id":5,"ok":"done"}\n']);
  });
});
