/**
 * Immortal in-memory SwarmProjection store.
 *
 * Dashboard lifetime: one reduction, bounded snapshot. Tearing this down
 * on route change is how Chat and SwarmPanel drifted. The pure reducer
 * is `swarmProjection.ts` (unit-testable without `window`).
 */

import { subscribeFeed } from "./swarmFeed";
import {
  applySwarmEvent,
  emptySwarmSnapshot,
  emptyUnreadCtx,
  hydrateUnread,
  type SwarmSnapshot,
  type UnreadCtx,
} from "./swarmProjection";
import type { MessageRecord } from "../api/types";

let snap: SwarmSnapshot = emptySwarmSnapshot;
let ctx: UnreadCtx = emptyUnreadCtx();
const listeners = new Set<() => void>();
let pumping = false;
let stopPump: (() => void) | null = null;

function emit() {
  for (const l of listeners) l();
}

function bumpReconnect() {
  snap = { ...snap, reconnectGen: snap.reconnectGen + 1 };
  emit();
}

/** Start the immortal pump. Safe to call repeatedly. */
export function ensureSwarmProjection() {
  if (pumping) return;
  pumping = true;
  stopPump = subscribeFeed({
    onEvent: (ev) => {
      const next = applySwarmEvent(snap, ev, ctx);
      if (next === snap) return;
      snap = next;
      emit();
    },
    onReconnect: bumpReconnect,
  });
}

export function getSwarmSnapshot(): SwarmSnapshot {
  return snap;
}

export function subscribeSwarmProjection(listener: () => void): () => void {
  ensureSwarmProjection();
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function hydrateSwarmUnread(rows: MessageRecord[]) {
  const { unreadByFrom, ctx: next } = hydrateUnread(rows, ctx);
  ctx = next;
  snap = { ...snap, unreadByFrom };
  emit();
}

/** Test-only: wipe module state. */
export function resetSwarmProjectionForTests() {
  stopPump?.();
  stopPump = null;
  snap = emptySwarmSnapshot;
  ctx = emptyUnreadCtx();
  listeners.clear();
  pumping = false;
}
