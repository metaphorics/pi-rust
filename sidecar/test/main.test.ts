/**
 * End-to-end subprocess test: the REAL entry point (`bun src/main.ts`) with
 * an extension that prints stray output at load time and inside handlers.
 * The protocol channel must stay pure NDJSON — invariant I2.
 */

import { describe, expect, test } from "bun:test";
import { join } from "node:path";

import { decodeFrame, encodeFrame } from "../src/protocol.ts";
import type { Envelope, JsonValue } from "../src/protocol.ts";
import { fromWire } from "../src/protocol.ts";
import { FIXTURES_DIR, makeInitParams } from "./harness.ts";

describe("sidecar subprocess", () => {
  test("stray extension stdout never reaches the protocol channel", async () => {
    const proc = Bun.spawn(["bun", join(import.meta.dir, "..", "src", "main.ts")], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
      cwd: join(import.meta.dir, ".."),
    });

    const frames: Envelope[] = [];
    let residual = "";
    const decoder = new TextDecoder();
    const strayBytes: string[] = [];
    const waiters: Array<{ match: (frame: Envelope) => boolean; resolve: (frame: Envelope) => void }> = [];

    const admit = (frame: Envelope) => {
      frames.push(frame);
      for (let i = waiters.length - 1; i >= 0; i--) {
        const waiter = waiters[i];
        if (waiter !== undefined && waiter.match(frame)) {
          waiters.splice(i, 1);
          waiter.resolve(frame);
        }
      }
    };

    const waitFor = (match: (frame: Envelope) => boolean): Promise<Envelope> => {
      const existing = frames.find(match);
      if (existing !== undefined) return Promise.resolve(existing);
      const { promise, resolve } = Promise.withResolvers<Envelope>();
      waiters.push({ match, resolve });
      return promise;
    };

    const collector = (async () => {
      for await (const chunk of proc.stdout) {
        residual += decoder.decode(chunk, { stream: true });
        let index = residual.indexOf("\n");
        while (index >= 0) {
          const line = residual.slice(0, index + 1);
          residual = residual.slice(index + 1);
          try {
            admit(decodeFrame(new TextEncoder().encode(line)));
          } catch {
            strayBytes.push(line);
          }
          index = residual.indexOf("\n");
        }
      }
    })();

    const send = (envelope: Envelope) => {
      proc.stdin.write(encodeFrame(envelope));
      proc.stdin.flush();
    };

    const init = makeInitParams({
      configuredPaths: [join(FIXTURES_DIR, "stray-stdout.ts"), join(FIXTURES_DIR, "simple.ts")],
    });
    send({ type: "req", id: 1, method: "lifecycle/init", params: fromWire<JsonValue>(init) });
    await waitFor((frame) => frame.type === "ev" && frame.method === "lifecycle/initialized");
    await waitFor((frame) => frame.type === "res" && frame.id === 1);

    // Fire an event whose handler prints stray output, then shut down.
    send({
      type: "ev",
      method: "event/notify",
      params: fromWire<JsonValue>({ event: { type: "agent_start" }, state: init.state }),
    });
    send({ type: "req", id: 2, method: "lifecycle/shutdown", params: {} });

    const exitCode = await proc.exited;
    await collector;

    expect(exitCode).toBe(0);
    expect(strayBytes).toEqual([]);
    expect(residual).toBe("");

    const methods = frames
      .filter((frame): frame is Envelope & { type: "ev" } => frame.type === "ev")
      .map((frame) => frame.method);
    expect(methods).toContain("lifecycle/hello");
    expect(methods).toContain("lifecycle/initialized");
    // Both extensions loaded despite the stray printing.
    const initialized = frames.find((frame) => frame.type === "ev" && frame.method === "lifecycle/initialized");
    const payload = fromWire<{ registrations: { tools: Array<{ name: string }> }; errors: unknown[] }>(
      initialized !== undefined && "params" in initialized ? initialized.params : null,
    );
    expect(payload.errors).toEqual([]);
    expect(payload.registrations.tools.map((tool) => tool.name)).toEqual(["echo_tool"]);
    // Responses for init and shutdown arrived on the protocol channel.
    const responseIds = frames.filter((frame) => frame.type === "res").map((frame) => frame.id);
    expect(responseIds.sort()).toEqual([1, 2]);
  }, 30000);
});
