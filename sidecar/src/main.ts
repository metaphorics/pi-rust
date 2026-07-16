/**
 * Sidecar entry point.
 *
 * ORDER MATTERS: the stdio guard must capture the real stdout writer before
 * any pi runtime code is even imported (invariant I2) — a static import of
 * host.ts would hoist pi's module evaluation ahead of the guard. The dynamic
 * import below is deliberate: it is the only way to sequence module
 * evaluation after the guard install.
 */

import { installStdioGuard, protocolWrite } from "./stdio-guard.ts";

export async function startSidecar(): Promise<void> {
  const sink = installStdioGuard();

  // Dynamic on purpose: see the module comment (guard-before-evaluation).
  const [{ attachHost }, { RpcPeer }, { createUi }] = await Promise.all([
    import("./host.ts"),
    import("./rpc.ts"),
    import("./ui-context.ts"),
  ]);

  const peer = new RpcPeer({
    write: protocolWrite,
    onTransportError: (error) => {
      // Diagnostics go to the captured sink, never the protocol channel.
      sink.chunks.push({ stream: "stderr", data: `[sidecar rpc] ${error.message}\n` });
    },
  });

  const host = attachHost({
    peer,
    onShutdown: () => {
      process.exit(0);
    },
    createUi: (runtime) => (runtime.hasUi ? createUi(runtime) : undefined),
  });

  process.stdin.on("data", (chunk: Buffer) => {
    peer.feed(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength));
  });
  process.stdin.on("end", () => {
    // Host went away; nothing left to serve.
    process.exit(0);
  });
  process.stdin.resume();

  host.sendHello();
}

if (import.meta.main) {
  await startSidecar();
}
