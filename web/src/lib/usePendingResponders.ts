import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { AgentInfo, AgentLiveState, MessageRecord } from "../api/types";
import {
  derivePendingResponders,
  deriveVanishedTurns,
  type PendingResponder,
  type VanishedTurn,
} from "./pendingResponders";
import { dlog } from "./debugLog";

/**
 * The pending-responder / vanished-turn state machine, lifted out of
 * MessagesPanel. Owns:
 *  - interruptedTriggers: the optimistic-clear set, keyed agentId → the
 *    server-assigned id of the cancelled trigger. Clock-free: a later message
 *    has a new id so it re-shows; a stale entry never matches a future turn.
 *  - the 5s tick that re-evaluates the give-up backstop with no new WS traffic;
 *  - both derivations (via the pure, unit-tested lib/pendingResponders);
 *  - the once-per-turn "vanished" console.warn breadcrumb;
 *  - markInterrupted, the mutator the stop-controls / composer-keymap call.
 *
 * The whole interruptedTriggers ↔ pendingResponders ↔ markInterrupted cycle
 * lives in here, so React only has to tolerate plain state feeding a memo —
 * exactly as it did inline. The component just consumes the three returns.
 */
export function usePendingResponders(input: {
  items: MessageRecord[];
  aliveForInference: AgentInfo[];
  agentLiveStateById: Record<string, AgentLiveState> | undefined;
}): {
  pendingResponders: PendingResponder[];
  vanishedTurns: VanishedTurn[];
  markInterrupted: (agentId: string) => void;
} {
  const { items, aliveForInference, agentLiveStateById } = input;

  const [interruptedTriggers, setInterruptedTriggers] = useState<
    Record<string, number>
  >({});

  // Tick every 5s so the give-up backstop is re-evaluated even when no new
  // events arrive on /ws/swarm.
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const i = window.setInterval(() => setTick((t) => t + 1), 5000);
    return () => window.clearInterval(i);
  }, []);

  const pendingResponders = useMemo(
    () =>
      derivePendingResponders({
        items,
        aliveForInference,
        agentLiveStateById,
        interruptedTriggers,
        now: Date.now(),
      }),
    // tick is purposeful — re-evaluates the give-up cutoff over time even with
    // no new WS events (Date.now() is read fresh each tick). The inference
    // itself lives in lib/pendingResponders (unit-tested); this memo only owns
    // the React deps.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [items, aliveForInference, agentLiveStateById, interruptedTriggers, tick],
  );

  const vanishedTurns = useMemo(
    () =>
      deriveVanishedTurns({ items, agentLiveStateById, interruptedTriggers }),
    [items, agentLiveStateById, interruptedTriggers],
  );

  // Diagnostic breadcrumb: log the moment a captain's turn vanishes with no
  // reply, once per turn, for after-the-fact debugging of "正在…然后突然没了".
  const loggedVanishedRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    for (const v of vanishedTurns) {
      const key = `${v.agentId}:${v.trigger.id}`;
      if (loggedVanishedRef.current.has(key)) continue;
      loggedVanishedRef.current.add(key);
      dlog("turn.vanished", {
        agentId: v.agentId,
        triggerId: v.trigger.id,
        state: agentLiveStateById?.[v.agentId]?.state ?? null,
        reason: v.reason ?? null,
      });
      console.warn(
        `[swarmx] 规划本轮中断:agent=${v.agentId} 收到任务#${v.trigger.id} 后进入 ` +
          `${agentLiveStateById?.[v.agentId]?.state ?? "?"} 终态,未产出回复` +
          (v.reason ? ` · 原因:${v.reason}` : ""),
      );
    }
  }, [vanishedTurns, agentLiveStateById]);

  // Mark the agent's CURRENT pending turn cancelled, keyed by that trigger's
  // server-assigned id (looked up from pendingResponders at click time).
  // Clock-free and self-clearing — a later message has a new id, so it re-shows.
  const markInterrupted = useCallback(
    (agentId: string) => {
      const trig = pendingResponders.find((p) => p.agentId === agentId)?.trigger;
      if (trig) {
        setInterruptedTriggers((m) => ({ ...m, [agentId]: trig.id }));
      }
    },
    [pendingResponders],
  );

  return { pendingResponders, vanishedTurns, markInterrupted };
}
