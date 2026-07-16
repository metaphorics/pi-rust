/**
 * In-memory mirror of the Rust-owned session (single-writer invariant I1).
 *
 * Only Rust writes real session files. The sidecar rehydrates pi's REAL
 * `SessionManager.inMemory` from `session/sync` notifications so every
 * `ReadonlySessionManager` getter (getBranch, buildContextEntries, getTree,
 * ...) runs pi's own code with zero reimplementation.
 *
 * PIN NOTE (0.80.7): rehydration assigns the SessionManager's private runtime
 * fields (`fileEntries`, `leafId`, `sessionId`, `sessionFile`) and calls its
 * private `_buildIndex()`. These names are verified against the pinned npm
 * dist (`dist/core/session-manager.js`); the exact-version pin plus this
 * test-covered seam make the deep reach safe. Do NOT reimplement the getters.
 */

import { SessionManager } from "@earendil-works/pi-coding-agent";

import type { JsonValue, SessionSnapshotDto, SessionSyncDto } from "./protocol.ts";

/** The private 0.80.7 internals rehydration relies on (see pin note above). */
interface SessionManagerInternals {
  fileEntries: unknown[];
  leafId: string | null;
  sessionId: string;
  sessionFile: string | undefined;
  _buildIndex(): void;
}

interface SessionHeaderLike {
  type?: string;
  id?: string;
}

export class SessionMirror {
  private sm: SessionManager;
  private epoch = -1;
  private cwd: string;
  /** Optimistic entries appended locally, awaiting the authoritative sync. */
  private optimistic: unknown[] = [];
  /** Reported when an incremental sync arrives out of order (mirror kept). */
  onStale?: (expectedEpoch: number, receivedEpoch: number) => void;

  constructor(cwd: string, snapshot?: SessionSnapshotDto) {
    this.cwd = cwd;
    this.sm = SessionManager.inMemory(cwd);
    if (snapshot !== undefined) {
      this.epoch = snapshot.epoch;
      this.rebuild(snapshot.header, snapshot.entries, snapshot.leafId, snapshot.sessionFile);
    }
  }

  /** pi's real SessionManager, served to the runner and extension contexts. */
  get sessionManager(): SessionManager {
    return this.sm;
  }

  get currentEpoch(): number {
    return this.epoch;
  }

  /** Apply a `session/sync` notification (epoch-guarded). */
  sync(params: SessionSyncDto): void {
    if (params.entries !== undefined) {
      // Full resync: authoritative snapshot replaces everything, including
      // any optimistic entries still in flight. A combined message may also
      // carry `appended`; the final content is entries ++ appended.
      this.epoch = params.epoch;
      this.optimistic = [];
      this.rebuild(
        params.header,
        params.appended === undefined ? params.entries : [...params.entries, ...params.appended],
        params.leafId,
        params.sessionFile,
      );
      return;
    }
    if (params.appended !== undefined) {
      // Producer contract: every session/sync bumps the epoch by one.
      // Anything else means a dropped or reordered notification — leave the
      // mirror untouched; Rust recovers with a full `entries` resync.
      if (params.epoch !== this.epoch + 1) {
        this.onStale?.(this.epoch + 1, params.epoch);
        return;
      }
      this.epoch = params.epoch;
      const internals = this.internals();
      // Authoritative copies of entries we appended optimistically replace
      // the local ones (ids differ; Rust allocates the real ids). Multiset
      // match: each appended entry consumes at most ONE optimistic entry —
      // identical duplicate appends are valid and must all survive.
      if (this.optimistic.length > 0) {
        const budget = new Map<string, number>();
        for (const entry of params.appended) {
          const key = optimisticKey(entry);
          budget.set(key, (budget.get(key) ?? 0) + 1);
        }
        const drop = new Set<unknown>();
        for (const local of this.optimistic) {
          const key = optimisticKey(local);
          const remaining = budget.get(key) ?? 0;
          if (remaining > 0) {
            budget.set(key, remaining - 1);
            drop.add(local);
          }
        }
        if (drop.size > 0) {
          internals.fileEntries = internals.fileEntries.filter((entry) => !drop.has(entry));
          this.optimistic = this.optimistic.filter((entry) => !drop.has(entry));
        }
      }
      internals.fileEntries.push(...params.appended);
      internals._buildIndex();
      internals.leafId = params.leafId ?? internals.leafId;
      internals.sessionFile = params.sessionFile;
    }
  }

  /**
   * Deterministic local apply for SYNC-VOID session actions so read-after-
   * write within one handler observes the write (see state-mirror docs).
   * Returns the locally allocated id (superseded by the next sync).
   */
  optimisticCustomEntry(customType: string, data: unknown): string {
    const id = this.sm.appendCustomEntry(customType, data);
    const entry = this.sm.getEntry(id);
    if (entry !== undefined) this.optimistic.push(entry);
    return id;
  }

  optimisticSessionInfo(name: string): void {
    const id = this.sm.appendSessionInfo(name);
    const entry = this.sm.getEntry(id);
    if (entry !== undefined) this.optimistic.push(entry);
  }

  optimisticLabelChange(targetId: string, label: string | undefined): void {
    const id = this.sm.appendLabelChange(targetId, label);
    const entry = this.sm.getEntry(id);
    if (entry !== undefined) this.optimistic.push(entry);
  }

  /** Throwaway in-memory manager for `newSession`/`fork` setup() callbacks. */
  createSetupSession(): SessionManager {
    return SessionManager.inMemory(this.cwd);
  }

  private rebuild(
    header: JsonValue | undefined,
    entries: JsonValue[],
    leafId: string | null,
    sessionFile: string,
  ): void {
    // In place: the ExtensionRunner (and every extension context) holds a
    // reference to this SessionManager instance; a session switch must not
    // strand them on stale data.
    const internals = this.internals();
    const existingHeader = internals.fileEntries[0];
    internals.fileEntries = [header ?? existingHeader ?? null, ...entries].filter(
      (entry) => entry !== null,
    );
    internals._buildIndex();
    internals.leafId = leafId;
    internals.sessionFile = sessionFile;
    const headerLike = (header ?? existingHeader) as SessionHeaderLike | undefined;
    if (headerLike !== undefined && typeof headerLike.id === "string") {
      internals.sessionId = headerLike.id;
    }
  }

  private internals(): SessionManagerInternals {
    return freshInternals(this.sm);
  }
}

/**
 * Escape hatch for the pinned private fields (see PIN NOTE). SessionManager
 * declares these fields `private`, so a direct cast is rejected; widening to
 * `object` first is the documented exception for reaching pinned third-party
 * internals. Runtime presence is asserted so a version drift fails loudly
 * instead of corrupting the mirror.
 */
function freshInternals(sm: SessionManager): SessionManagerInternals {
  const record: object = sm;
  const internals = record as Partial<SessionManagerInternals>;
  if (!Array.isArray(internals.fileEntries) || typeof internals._buildIndex !== "function") {
    throw new Error(
      "pinned SessionManager internals changed (fileEntries/_buildIndex missing); re-verify against the npm dist",
    );
  }
  return internals as SessionManagerInternals;
}

/**
 * Identity key used to match an optimistic local entry with its
 * authoritative copy from Rust (ids differ; type + payload must agree).
 */
function optimisticKey(entry: unknown): string {
  if (typeof entry !== "object" || entry === null || Array.isArray(entry)) return JSON.stringify(entry);
  const { id: _id, parentId: _parentId, timestamp: _timestamp, ...rest } = entry as Record<string, unknown>;
  return JSON.stringify(rest);
}
