/**
 * useSwarmFeed — React wrapper around the shared `/ws/swarm` transport
 * (`lib/swarmFeed`). Prefer `useSwarmProjection` / `useSwarmField` when you
 * need reduced state. Keep this hook for adapters that must see the raw
 * event (notifications, wizard scan-done, desktop notify).
 */

import { useEffect, useRef, useState } from "react";
import type { SwarmEvent } from "../api/types";
import {
  getFeedStatus,
  subscribeFeed,
  subscribeFeedStatus,
  type SwarmFeedStatus,
} from "../lib/swarmFeed";

export type { SwarmFeedStatus };

interface Options {
  onEvent: (ev: SwarmEvent) => void;
  onReconnect?: () => void;
}

export function useSwarmFeedStatus(): SwarmFeedStatus {
  const [s, setS] = useState<SwarmFeedStatus>(getFeedStatus());
  useEffect(() => subscribeFeedStatus(setS), []);
  return s;
}

export function useSwarmFeed({ onEvent, onReconnect }: Options): SwarmFeedStatus {
  const [s, setS] = useState<SwarmFeedStatus>(
    getFeedStatus() === "closed" ? "connecting" : getFeedStatus(),
  );
  const cbRef = useRef({ onEvent, onReconnect });
  cbRef.current.onEvent = onEvent;
  cbRef.current.onReconnect = onReconnect;

  useEffect(() => {
    const unsubStatus = subscribeFeedStatus(setS);
    const unsub = subscribeFeed({
      onEvent: (ev) => cbRef.current.onEvent(ev),
      onReconnect: () => cbRef.current.onReconnect?.(),
    });
    return () => {
      unsub();
      unsubStatus();
    };
  }, []);

  return s;
}
