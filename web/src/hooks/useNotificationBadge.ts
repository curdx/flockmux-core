/**
 * AppShell 顶栏铃铛未读数 — 不要重新实现一套通知 inbox（那
 * 是 /notifications route 的活），只回答一个问题："还有几条没读"。
 *
 * 方向必须跟通知中心一致。早先红点只用本会话 WS 事件，刷新后 61 条
 * 未读在中心、铃铛却只有空红点/空白弹层。现在 mount 时按中心同款
 * `msg-<id>` / `bb-<path>` seed 历史，未读 = knownIds − readSet。
 *
 * 瞄一眼弹层 ≠ 已读。只有中心「标为已读」才写 READ_KEY。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/api/http";
import { useSwarmFeed } from "./useSwarmFeed";
import {
  isHiddenWake,
  isNoisyBlackboard,
  NOTIF_READ_EVENT,
  NOTIF_READ_KEY,
} from "@/lib/notif";

function readReadSet(): Set<string> {
  try {
    const raw = window.localStorage.getItem(NOTIF_READ_KEY);
    if (!raw) return new Set();
    return new Set(JSON.parse(raw) as string[]);
  } catch {
    return new Set();
  }
}

export function useNotificationBadge() {
  const seenIdsRef = useRef<Set<string>>(new Set());
  const [, bump] = useState(0);
  const [readSet, setReadSet] = useState<Set<string>>(readReadSet);
  const [seeded, setSeeded] = useState(false);

  const remember = useCallback((id: string) => {
    if (seenIdsRef.current.has(id)) return;
    seenIdsRef.current.add(id);
    bump((n) => n + 1);
  }, []);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [msgs, bb] = await Promise.all([
          api.listMessages({ limit: 200 }),
          api.listBlackboard(),
        ]);
        if (cancelled) return;
        for (const m of msgs) {
          if (isHiddenWake(m) || m.from_agent === "cron") continue;
          seenIdsRef.current.add(`msg-${m.id}`);
        }
        for (const e of bb) {
          if (isNoisyBlackboard(e.path)) continue;
          seenIdsRef.current.add(`bb-${e.path}`);
        }
        bump((n) => n + 1);
      } catch {
        /* best-effort — live feed still works */
      } finally {
        if (!cancelled) setSeeded(true);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useSwarmFeed({
    onEvent: (ev) => {
      let id: string | null = null;
      if (ev.type === "message") {
        if (isHiddenWake(ev) || ev.from_agent === "cron") return;
        id = `msg-${ev.id}`;
      } else if (ev.type === "blackboard_changed") {
        if (isNoisyBlackboard(ev.path)) return;
        id = `bb-${ev.path}`;
      }
      if (id == null) return;
      remember(id);
    },
  });

  const syncRead = useCallback(() => setReadSet(readReadSet()), []);

  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === NOTIF_READ_KEY) syncRead();
    };
    window.addEventListener("storage", onStorage);
    window.addEventListener(NOTIF_READ_EVENT, syncRead);
    return () => {
      window.removeEventListener("storage", onStorage);
      window.removeEventListener(NOTIF_READ_EVENT, syncRead);
    };
  }, [syncRead]);

  let unseenCount = 0;
  if (seeded) {
    for (const id of seenIdsRef.current) {
      if (!readSet.has(id)) unseenCount += 1;
    }
  }

  return {
    unseenCount,
    hasUnseen: unseenCount > 0,
  };
}
