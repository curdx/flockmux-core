/**
 * Auto-nudge agents that are "stalled" (alive, overdue unread mail, no
 * activity) so the user never has to click "催一下 / 唤醒".
 *
 * Cooldown per agent avoids wake storms if the flag stays set while a slow
 * kick is still in flight. Failures are silent — the next tick retries after
 * the cooldown; NeedsYou only surfaces error/handoff that need a human.
 */

import { useEffect, useRef } from "react";
import { api } from "@/api/http";
import { deriveNeedsYou } from "@/lib/needsYou";
import type { AgentInfo, AgentLiveState, MessageRecord } from "@/api/types";

const COOLDOWN_MS = 120_000;

export function useAutoNudgeStalled(args: {
  members: AgentInfo[];
  liveById: Record<string, AgentLiveState | undefined>;
  messages: MessageRecord[];
}): void {
  const { members, liveById, messages } = args;
  const lastNudgeAt = useRef<Map<string, number>>(new Map());

  useEffect(() => {
    const now = Date.now();
    const stalled = deriveNeedsYou(members, liveById, messages, now).filter(
      (i) => i.kind === "stalled",
    );
    for (const item of stalled) {
      const id = item.agent.agent_id;
      const prev = lastNudgeAt.current.get(id) ?? 0;
      if (now - prev < COOLDOWN_MS) continue;
      lastNudgeAt.current.set(id, now);
      void api.wakeAgent(id).catch(() => {
        // Allow a sooner retry on hard failure (network / gone).
        lastNudgeAt.current.set(id, now - COOLDOWN_MS + 15_000);
      });
    }
  }, [members, liveById, messages]);
}
