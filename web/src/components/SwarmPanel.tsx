/**
 * SwarmPanel — right-hand side drawer with three tabs: messages, blackboard,
 * recordings. Reads SwarmProjection (the dashboard's one /ws/swarm reduction).
 *
 * Unread uses the same `countsAsUserUnread` contract as the workspace shell —
 * wake/system/agent↔agent traffic is not a user badge.
 */

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSwarmFeedStatus } from "../hooks/useSwarmFeed";
import {
  useLiveBbChanges,
  useSwarmRefresh,
  useSwarmSnapshot,
} from "../hooks/useSwarmProjection";
import { hydrateSwarmUnread } from "../lib/swarmProjectionStore";
import { api } from "../api/http";
import { MessagesPanel } from "./MessagesPanel";
import { BlackboardPanel } from "./BlackboardPanel";
import { RecordingsPanel } from "./RecordingsPanel";

// The collaboration graph lives in the primary product at /chat/:wsId/dag
// (ReactFlow + dagre). The old hand-rolled SVG GraphPanel that used to be a
// tab here was deleted — there's now ONE DAG implementation (edge logic in
// lib/dagEdgeDerivation), so the two can no longer drift.
type Tab = "messages" | "blackboard" | "recordings";

// 显示名：tab 内部 key 仍用英文（避免改一堆 switch/比较），
// 渲染时通过 i18n 映射到本地化文案，中文原文作为 zh 兜底。
const TAB_LABELS: Record<Tab, { key: string; zh: string }> = {
  messages: { key: "swarm.tabs.messages", zh: "消息" },
  blackboard: { key: "swarm.tabs.blackboard", zh: "共享区" },
  recordings: { key: "swarm.tabs.recordings", zh: "录像" },
};

export function SwarmPanel() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<Tab>("messages");
  const [liveChange, setLiveChange] = useState<{
    path: string;
    agent_id: string | null;
    op: string;
  } | null>(null);
  const snap = useSwarmSnapshot();
  const status = useSwarmFeedStatus();

  const recomputeUnread = useCallback(async () => {
    try {
      hydrateSwarmUnread(await api.listMessages({ limit: 200 }));
    } catch {
      // best-effort; leave existing state
    }
  }, []);

  useEffect(() => {
    void recomputeUnread();
  }, [recomputeUnread]);

  useSwarmRefresh((s) => s.reconnectGen, () => {
    void recomputeUnread();
  });
  useLiveBbChanges((c) => {
    setLiveChange({ path: c.path, agent_id: c.agent_id, op: c.op });
  });

  const totalUnread = Object.values(snap.unreadByFrom).reduce((a, b) => a + b, 0);

  return (
    <aside style={container}>
      <div style={tabBar}>
        {(["messages", "blackboard", "recordings"] as Tab[]).map((tabKey) => (
          <button
            key={tabKey}
            onClick={() => setTab(tabKey)}
            style={{
              ...tabButton,
              background: tab === tabKey ? "#1e3a8a" : "transparent",
              color: tab === tabKey ? "#e2e8f0" : "#94a3b8",
            }}
          >
            {t(TAB_LABELS[tabKey].key, { defaultValue: TAB_LABELS[tabKey].zh })}
            {tabKey === "messages" && totalUnread > 0 && (
              <span style={tabBadge}>{totalUnread}</span>
            )}
          </button>
        ))}
        <span
          style={statusDot}
          title={t("swarm.wsStatus", {
            status,
            defaultValue: "协作 WS：{{status}}",
          })}
        >
          <span
            style={{
              display: "inline-block",
              width: 8,
              height: 8,
              borderRadius: "50%",
              background:
                status === "open"
                  ? "#22c55e"
                  : status === "connecting"
                    ? "#fbbf24"
                    : "#ef4444",
            }}
          />
        </span>
      </div>
      <div style={body}>
        {tab === "messages" && (
          <MessagesPanel
            liveMessages={snap.liveMessages}
            liveRead={snap.liveRead}
            unreadByFrom={snap.unreadByFrom}
          />
        )}
        {tab === "blackboard" && <BlackboardPanel liveChange={liveChange} />}
        {tab === "recordings" && (
          <RecordingsPanel refreshTick={snap.recordingsGen + snap.reconnectGen} />
        )}
      </div>
    </aside>
  );
}

const container: React.CSSProperties = {
  width: 360,
  borderLeft: "1px solid #374151",
  background: "#0f172a",
  display: "flex",
  flexDirection: "column",
  minHeight: 0,
};

const tabBar: React.CSSProperties = {
  display: "flex",
  borderBottom: "1px solid #374151",
  background: "#1f2937",
};

const tabButton: React.CSSProperties = {
  flex: 1,
  border: "none",
  borderRight: "1px solid #374151",
  padding: "6px 0",
  fontSize: 12,
  cursor: "pointer",
  display: "inline-flex",
  alignItems: "center",
  justifyContent: "center",
  gap: 6,
};

const tabBadge: React.CSSProperties = {
  background: "#dc2626",
  color: "#fff",
  borderRadius: 8,
  padding: "0 6px",
  fontSize: 10,
  fontWeight: 600,
  lineHeight: "14px",
};

const statusDot: React.CSSProperties = {
  display: "flex",
  alignItems: "center",
  padding: "0 8px",
};

const body: React.CSSProperties = {
  flex: 1,
  minHeight: 0,
  display: "flex",
  flexDirection: "column",
};
