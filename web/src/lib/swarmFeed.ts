/**
 * Shared `/ws/swarm` transport. Not the reduction — that's `swarmProjection`.
 *
 * ONE socket for the page. Subscribers multiplex; linger-close on the last
 * unsubscribe so a route remount doesn't bounce the handshake.
 */

import type { SwarmEvent } from "../api/types";
import { WS_HOST, WS_PROTO } from "./apiBase";

export type SwarmFeedStatus = "connecting" | "open" | "closed";

export interface SwarmFeedSub {
  onEvent: (ev: SwarmEvent) => void;
  onReconnect?: () => void;
}

const BACKOFF_INITIAL_MS = 200;
const BACKOFF_MAX_MS = 4000;
const LINGER_CLOSE_MS = 5000;

const subs = new Set<SwarmFeedSub>();
const statusListeners = new Set<(s: SwarmFeedStatus) => void>();
let ws: WebSocket | null = null;
let status: SwarmFeedStatus = "closed";
let retry = BACKOFF_INITIAL_MS;
let reconnectTimer: number | null = null;
let lingerTimer: number | null = null;

function setStatus(s: SwarmFeedStatus) {
  status = s;
  for (const l of statusListeners) l(s);
}

function connect() {
  if (
    ws &&
    (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)
  ) {
    return;
  }
  setStatus("connecting");
  const next = new WebSocket(`${WS_PROTO}//${WS_HOST}/ws/swarm`);
  ws = next;

  next.onopen = () => {
    retry = BACKOFF_INITIAL_MS;
    setStatus("open");
    for (const s of subs) {
      try {
        s.onReconnect?.();
      } catch (err) {
        console.warn("swarm onReconnect threw", err);
      }
    }
  };

  next.onmessage = (msg) => {
    if (typeof msg.data !== "string") return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(msg.data);
    } catch {
      return;
    }
    if (!parsed || typeof (parsed as { type?: unknown }).type !== "string") {
      return;
    }
    for (const s of subs) {
      try {
        s.onEvent(parsed as SwarmEvent);
      } catch (err) {
        console.warn("swarm event handler threw", err, parsed);
      }
    }
  };

  next.onclose = () => {
    if (ws === next) ws = null;
    setStatus("closed");
    if (subs.size === 0) return;
    const delay = retry;
    retry = Math.min(retry * 2, BACKOFF_MAX_MS);
    if (reconnectTimer !== null) window.clearTimeout(reconnectTimer);
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, delay);
  };

  next.onerror = () => {
    /* onclose fires next */
  };
}

function ensureConnected() {
  if (lingerTimer !== null) {
    window.clearTimeout(lingerTimer);
    lingerTimer = null;
  }
  connect();
}

function maybeDisconnect() {
  if (subs.size > 0) return;
  if (lingerTimer !== null) window.clearTimeout(lingerTimer);
  lingerTimer = window.setTimeout(() => {
    lingerTimer = null;
    if (subs.size > 0) return;
    if (reconnectTimer !== null) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (ws) {
      const dead = ws;
      ws = null;
      dead.onopen = null;
      dead.onmessage = null;
      dead.onclose = null;
      dead.onerror = null;
      try {
        dead.close();
      } catch {
        /* ignore */
      }
    }
    setStatus("closed");
  }, LINGER_CLOSE_MS);
}

export function getFeedStatus(): SwarmFeedStatus {
  return status;
}

export function subscribeFeedStatus(
  listener: (s: SwarmFeedStatus) => void,
): () => void {
  statusListeners.add(listener);
  return () => {
    statusListeners.delete(listener);
  };
}

/** Joining an already-open socket fires `onReconnect` immediately. */
export function subscribeFeed(sub: SwarmFeedSub): () => void {
  subs.add(sub);
  ensureConnected();
  if (status === "open") {
    try {
      sub.onReconnect?.();
    } catch {
      /* ignore */
    }
  }
  return () => {
    subs.delete(sub);
    maybeDisconnect();
  };
}
