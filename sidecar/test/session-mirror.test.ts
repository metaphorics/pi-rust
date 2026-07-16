/**
 * Session mirror unit tests: rehydration into pi's REAL SessionManager,
 * epoch guarding, optimistic-entry reconciliation, and setup sessions.
 */

import { describe, expect, test } from "bun:test";

import { SessionMirror } from "../src/session-mirror.ts";
import type { JsonObject } from "../src/protocol.ts";

let nextId = 0;

function entry(id: string, parentId: string | null, extra?: JsonObject): JsonObject {
  return {
    type: "message",
    id,
    parentId,
    timestamp: `2026-07-16T00:00:0${(nextId++ % 10).toString()}.000Z`,
    message: { role: "user", content: `content-${id}` },
    ...extra,
  };
}

const HEADER: JsonObject = { type: "session", version: 3, id: "sess-1", timestamp: "2026-07-16T00:00:00.000Z", cwd: "/tmp/mirror-test" };

describe("session mirror hydration", () => {
  test("snapshot hydration serves pi's real getters", () => {
    const mirror = new SessionMirror("/tmp/mirror-test", {
      epoch: 3,
      sessionFile: "/sessions/live.jsonl",
      header: HEADER,
      entries: [entry("e1", null), entry("e2", "e1"), entry("e3", "e2")],
      leafId: "e3",
    });
    const sm = mirror.sessionManager;
    expect(mirror.currentEpoch).toBe(3);
    expect(sm.getLeafId()).toBe("e3");
    expect(sm.getEntries().map((e) => e.id)).toEqual(["e1", "e2", "e3"]);
    expect(sm.getBranch().map((e) => e.id)).toEqual(["e1", "e2", "e3"]);
    const tree = sm.getTree();
    expect(tree.length).toBe(1);
    expect(tree[0]?.entry.id).toBe("e1");
    expect(sm.buildContextEntries().map((e) => e.id)).toEqual(["e1", "e2", "e3"]);
  });

  test("incremental appended sync with successor epoch extends the tree", () => {
    const mirror = new SessionMirror("/tmp/mirror-test", {
      epoch: 1,
      sessionFile: "/sessions/live.jsonl",
      header: HEADER,
      entries: [entry("e1", null)],
      leafId: "e1",
    });
    mirror.sync({
      epoch: 2,
      sessionFile: "/sessions/live.jsonl",
      appended: [entry("e2", "e1")],
      leafId: "e2",
    });
    expect(mirror.currentEpoch).toBe(2);
    expect(mirror.sessionManager.getLeafId()).toBe("e2");
    expect(mirror.sessionManager.getBranch().map((e) => e.id)).toEqual(["e1", "e2"]);
  });

  test("out-of-order incremental sync is rejected and reported", () => {
    const mirror = new SessionMirror("/tmp/mirror-test", {
      epoch: 1,
      sessionFile: "/sessions/live.jsonl",
      header: HEADER,
      entries: [entry("e1", null)],
      leafId: "e1",
    });
    const stale: Array<[number, number]> = [];
    mirror.onStale = (expected, received) => stale.push([expected, received]);
    mirror.sync({
      epoch: 4, // expected 2
      sessionFile: "/sessions/live.jsonl",
      appended: [entry("e9", "e1")],
      leafId: "e9",
    });
    expect(stale).toEqual([[2, 4]]);
    expect(mirror.currentEpoch).toBe(1);
    expect(mirror.sessionManager.getLeafId()).toBe("e1");
  });

  test("full resync replaces content and preserves the manager instance", () => {
    const mirror = new SessionMirror("/tmp/mirror-test", {
      epoch: 1,
      sessionFile: "/sessions/a.jsonl",
      header: HEADER,
      entries: [entry("e1", null)],
      leafId: "e1",
    });
    const before = mirror.sessionManager;
    mirror.sync({
      epoch: 9,
      sessionFile: "/sessions/b.jsonl",
      header: { ...HEADER, id: "sess-2" },
      entries: [entry("n1", null), entry("n2", "n1")],
      leafId: "n2",
    });
    // Same instance: the ExtensionRunner captured this reference at boot.
    expect(mirror.sessionManager).toBe(before);
    expect(mirror.currentEpoch).toBe(9);
    expect(mirror.sessionManager.getBranch().map((e) => e.id)).toEqual(["n1", "n2"]);
  });

  test("combined entries + appended applies both deterministically", () => {
    const mirror = new SessionMirror("/tmp/mirror-test");
    mirror.sync({
      epoch: 5,
      sessionFile: "/sessions/c.jsonl",
      header: HEADER,
      entries: [entry("e1", null)],
      appended: [entry("e2", "e1")],
      leafId: "e2",
    });
    expect(mirror.sessionManager.getBranch().map((e) => e.id)).toEqual(["e1", "e2"]);
  });

  test("optimistic entries reconcile as a multiset against authoritative copies", () => {
    const mirror = new SessionMirror("/tmp/mirror-test", {
      epoch: 1,
      sessionFile: "/sessions/live.jsonl",
      header: HEADER,
      entries: [entry("e1", null)],
      leafId: "e1",
    });
    // Two identical optimistic custom entries — both must survive dedup.
    mirror.optimisticCustomEntry("marker", { n: 1 });
    mirror.optimisticCustomEntry("marker", { n: 1 });
    expect(mirror.sessionManager.getEntries().length).toBe(3);

    // Rust confirms ONE of them; the other stays local.
    mirror.sync({
      epoch: 2,
      sessionFile: "/sessions/live.jsonl",
      appended: [
        {
          type: "custom",
          customType: "marker",
          data: { n: 1 },
          id: "real-1",
          parentId: "e1",
          timestamp: "2026-07-16T00:01:00.000Z",
        },
      ],
      leafId: "real-1",
    });
    const entries = mirror.sessionManager.getEntries();
    const markers = entries.filter((e) => e.type === "custom");
    expect(markers.length).toBe(2);
    expect(markers.some((e) => e.id === "real-1")).toBe(true);
  });

  test("read-after-write: appendEntry and session name visible immediately", () => {
    const mirror = new SessionMirror("/tmp/mirror-test", {
      epoch: 1,
      sessionFile: "/sessions/live.jsonl",
      header: HEADER,
      entries: [entry("e1", null)],
      leafId: "e1",
    });
    mirror.optimisticSessionInfo("My Session");
    expect(mirror.sessionManager.getSessionName()).toBe("My Session");
    const id = mirror.optimisticCustomEntry("todo-state", { items: [1, 2] });
    const stored = mirror.sessionManager.getEntry(id);
    expect(stored?.type).toBe("custom");
  });

  test("setup sessions are throwaway and capture produced entries", () => {
    const mirror = new SessionMirror("/tmp/mirror-test");
    const setup = mirror.createSetupSession();
    setup.appendCustomEntry("seed", { a: 1 });
    setup.appendSessionInfo("Seeded");
    expect(setup.getEntries().length).toBe(2);
    // The live mirror is untouched.
    expect(mirror.sessionManager.getEntries().length).toBe(0);
    expect(setup).not.toBe(mirror.sessionManager);
  });
});
