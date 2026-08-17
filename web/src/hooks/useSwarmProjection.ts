/**
 * React seam for SwarmProjection. Views select a field (generation counter
 * or slice) so they don't re-render on unrelated feed events.
 */

import { useEffect, useRef, useSyncExternalStore } from "react";
import type { LiveBbChange, SwarmSnapshot } from "../lib/swarmProjection";
import {
  ensureSwarmProjection,
  getSwarmSnapshot,
  subscribeSwarmProjection,
} from "../lib/swarmProjectionStore";

export function useSwarmSnapshot(): SwarmSnapshot {
  return useSyncExternalStore(
    subscribeSwarmProjection,
    getSwarmSnapshot,
    getSwarmSnapshot,
  );
}

export function useSwarmField<T>(select: (s: SwarmSnapshot) => T): T {
  const selectRef = useRef(select);
  selectRef.current = select;
  return useSyncExternalStore(
    subscribeSwarmProjection,
    () => selectRef.current(getSwarmSnapshot()),
    () => selectRef.current(getSwarmSnapshot()),
  );
}

/**
 * Call `refresh` when `select` changes after the first paint.
 * Mount-time fetch stays the caller's `useEffect`; this covers live gens
 * and reconnects.
 */
export function useSwarmRefresh(
  select: (s: SwarmSnapshot) => string | number,
  refresh: () => void,
) {
  ensureSwarmProjection();
  const token = useSwarmField(select);
  const refreshRef = useRef(refresh);
  refreshRef.current = refresh;
  const skip = useRef(true);
  useEffect(() => {
    if (skip.current) {
      skip.current = false;
      return;
    }
    refreshRef.current();
  }, [token]);
}

/**
 * Skip the current blackboard ring on mount (REST already loaded the view),
 * then call `onChange` for each *new* entry. A burst of N writes in one
 * tick is one snapshot jump — this walks all of them, not just the last.
 */
export function useLiveBbChanges(onChange: (c: LiveBbChange) => void) {
  const bbChanges = useSwarmField((s) => s.bbChanges);
  const primed = useRef(false);
  const lastId = useRef(0);
  const cb = useRef(onChange);
  cb.current = onChange;
  useEffect(() => {
    if (!primed.current) {
      primed.current = true;
      const last = bbChanges[bbChanges.length - 1];
      if (last) lastId.current = last.id;
      return;
    }
    for (const c of bbChanges) {
      if (c.id <= lastId.current) continue;
      lastId.current = c.id;
      cb.current(c);
    }
  }, [bbChanges]);
}
