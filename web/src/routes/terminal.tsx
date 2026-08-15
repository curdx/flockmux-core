/**
 * Terminal page (`/terminal`).
 *
 * An interactive `$SHELL` in the browser over /ws/terminal — for ad-hoc
 * commands, not a worker PTY. Minimal protocol: server sends binary PTY bytes,
 * we send keystrokes as binary + a `{type:"resize"}` JSON on fit.
 *
 * Per-workspace + persistent: the session id is keyed by the selected workspace
 * (`swarmx.terminal.session:<wsId>`, kept in sessionStorage), so each
 * workspace has its own shell that survives navigation/F5 (the server replays
 * scrollback on reattach). Switching the workspace picker tears down the view
 * and attaches that workspace's shell; a first spawn starts in its cwd.
 */
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Terminal as XtermTerminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { ShieldAlert, Terminal as TerminalIcon } from "lucide-react";
import { WS_HOST, WS_PROTO } from "@/lib/apiBase";
import { useToolWorkspaces } from "@/lib/useToolWorkspaces";
import { WorkspacePicker } from "@/components/WorkspacePicker";
import { Button } from "@/components/ui/button";

/** Stable per-tab, per-workspace terminal session id, so navigating away and
 *  back reattaches to the same server-side shell instead of spawning a fresh
 *  one — and each workspace keeps its own shell. */
const SID_PREFIX = "swarmx.terminal.session:";

/** Once the user confirms the "open a real shell" prompt, we remember it for the
 *  whole tab session so switching workspaces just rebuilds the connection instead
 *  of re-prompting every time. Honesty is preserved: the connect effect still
 *  surfaces a real backend/PTY failure even when this flag is set. */
const CONFIRMED_KEY = "swarmx.terminal.confirmed";
function readConfirmed(): boolean {
  try {
    return sessionStorage.getItem(CONFIRMED_KEY) === "1";
  } catch {
    return false;
  }
}
function persistConfirmed(): void {
  try {
    sessionStorage.setItem(CONFIRMED_KEY, "1");
  } catch {
    /* sessionStorage unavailable — fall back to in-memory state only */
  }
}
function terminalSessionId(wsId: string): string {
  const key = SID_PREFIX + wsId;
  try {
    let id = sessionStorage.getItem(key);
    if (!id) {
      id =
        typeof crypto !== "undefined" && crypto.randomUUID
          ? crypto.randomUUID()
          : `t-${Date.now()}-${Math.random().toString(36).slice(2)}`;
      sessionStorage.setItem(key, id);
    }
    return id;
  } catch {
    return `t-${Date.now()}`;
  }
}

export default function TerminalRoute() {
  const { t } = useTranslation();
  const { workspaces, wsId, setWsId, ready, error } = useToolWorkspaces();
  const hostRef = useRef<HTMLDivElement>(null);
  // Confirmed once per tab session: switching workspaces afterwards rebuilds the
  // connection without re-prompting (see CONFIRMED_KEY).
  const [armed, setArmed] = useState(readConfirmed);
  // Set true when the socket drops (onclose/onerror); drives the reconnect banner.
  const [wsClosed, setWsClosed] = useState(false);
  // Connection phase mirrored into a role="status" live region so screen readers
  // hear state changes (the xterm canvas is invisible to them).
  const [connPhase, setConnPhase] = useState<"connecting" | "connected" | "disconnected">(
    "connecting",
  );
  // Bumped by the reconnect button to re-run the connect effect even when
  // `armed` is already true.
  const [reconnectNonce, setReconnectNonce] = useState(0);
  const activeWs = workspaces.find((w) => w.id === wsId) ?? null;

  // Switching workspace just rebuilds the connection (the connect effect depends
  // on `wsId`); don't re-prompt. We only clear the stale "disconnected" banner so
  // the new shell starts from a clean status — a real failure re-sets it.
  useEffect(() => {
    setWsClosed(false);
    setConnPhase("connecting");
  }, [wsId]);

  useEffect(() => {
    // Wait until the workspace list resolved so we attach to the right shell.
    // If the backend is known-down, don't open a doomed socket — the armed
    // branch surfaces the backend-down banner honestly instead.
    if (!ready || !armed || error) return;
    const host = hostRef.current;
    if (!host) return;
    setWsClosed(false);

    const term = new XtermTerminal({
      fontSize: 13,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      cursorBlink: true,
      theme: { background: "#0d0d0d", foreground: "#f0f0f0" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    fit.fit();

    const wsParam = wsId ? `&workspace_id=${encodeURIComponent(wsId)}` : "";
    const ws = new WebSocket(
      `${WS_PROTO}//${WS_HOST}/ws/terminal?session=${encodeURIComponent(terminalSessionId(wsId))}${wsParam}`,
    );
    ws.binaryType = "arraybuffer";

    const sendResize = () => {
      try {
        fit.fit();
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
        }
      } catch {
        /* ignore */
      }
    };

    ws.onopen = () => {
      setConnPhase("connected");
      // Clear any stale "disconnected" banner a prior socket's late onclose may
      // have set — a live open connection is the ground truth.
      setWsClosed(false);
      term.focus();
      sendResize();
    };
    ws.onmessage = (e) => {
      if (typeof e.data === "string") term.write(e.data);
      else term.write(new Uint8Array(e.data as ArrayBuffer));
    };
    ws.onerror = () => {
      term.write(
        `\r\n\x1b[31m[${t("terminal.connError", { defaultValue: "连接出错" })}]\x1b[0m\r\n`,
      );
      setConnPhase("disconnected");
      setWsClosed(true);
    };
    ws.onclose = () => {
      term.write("\r\n\x1b[90m[session closed]\x1b[0m\r\n");
      setConnPhase("disconnected");
      setWsClosed(true);
    };

    const enc = new TextEncoder();
    const dataDisp = term.onData((d) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(enc.encode(d));
      } else {
        // Don't silently swallow keystrokes — tell the user nothing was sent.
        term.write(
          `\r\n\x1b[33m[${t("terminal.notConnectedInputDropped", { defaultValue: "未连接，输入未发送" })}]\x1b[0m\r\n`,
        );
        setConnPhase("disconnected");
        setWsClosed(true);
      }
    });

    const ro = new ResizeObserver(() => sendResize());
    ro.observe(host);

    return () => {
      ro.disconnect();
      dataDisp.dispose();
      // Detach handlers BEFORE closing. A WebSocket close event dispatches
      // asynchronously, so without this a torn-down socket's onclose/onerror
      // could fire after the next socket is already open and write a FALSE
      // "disconnected" into connPhase — which now drives a role=status live
      // region, so a screen reader would announce a connection-lost over a
      // healthy terminal. (Same guard XtermPane uses via its `disposed` flag.)
      ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
      ws.close();
      term.dispose();
    };
  }, [wsId, ready, armed, error, reconnectNonce]);

  return (
    <div className="flex h-full flex-col">
      <header className="flex items-center gap-2 border-b border-border-subtle px-4 py-2">
        <span className="font-display text-sm text-foreground-primary">{t("nav.terminal")}</span>
        {workspaces.length > 0 && (
          <WorkspacePicker
            className="ml-auto"
            workspaces={workspaces}
            value={wsId}
            onChange={setWsId}
          />
        )}
      </header>
      {armed ? (
        <div className="flex min-h-0 flex-1 flex-col">
          {/* Mirror connection state into an sr-only live region so screen readers
              announce connect/disconnect — the xterm canvas is invisible to them. */}
          <span role="status" aria-live="polite" className="sr-only">
            {error
              ? t("terminal.statusFailed", { defaultValue: "无法连接到后端" })
              : connPhase === "connected"
                ? t("terminal.statusConnected", { defaultValue: "终端已连接" })
                : connPhase === "disconnected"
                  ? t("terminal.statusDisconnected", { defaultValue: "终端连接已断开" })
                  : t("terminal.statusConnecting", { defaultValue: "正在连接终端…" })}
          </span>
          {(wsClosed || error) && (
            <div className="flex items-center gap-3 border-b border-border-subtle bg-status-warning-soft px-4 py-2">
              <span className="font-caption text-xs text-status-warning">
                {error
                  ? t("terminal.backendDown", {
                      defaultValue:
                        "暂时打不开终端。若顶部有「重启」请点一下，或重新打开应用。",
                    })
                  : t("terminal.disconnected", { defaultValue: "连接已断开" })}
              </span>
              <Button
                size="sm"
                variant="outline"
                className="ml-auto gap-1.5"
                disabled={!!error}
                onClick={() => {
                  setWsClosed(false);
                  setConnPhase("connecting");
                  setReconnectNonce((n) => n + 1);
                }}
              >
                <TerminalIcon className="size-3.5" />
                {t("terminal.reconnect", { defaultValue: "重新连接" })}
              </Button>
            </div>
          )}
          <div
            ref={hostRef}
            role="group"
            aria-label={t("terminal.hostLabel", { defaultValue: "终端" })}
            className="min-h-0 flex-1 overflow-hidden bg-[#0d0d0d] p-2"
          />
        </div>
      ) : (
        <div className="flex min-h-0 flex-1 items-center justify-center bg-surface-primary px-6">
          <section className="flex w-full max-w-lg flex-col gap-4 rounded-lg border border-border-subtle bg-surface-elevated p-5">
            <div className="flex items-start gap-3">
              <span className="flex size-10 shrink-0 items-center justify-center rounded-md bg-status-warning-soft text-status-warning">
                <ShieldAlert className="size-5" />
              </span>
              <div className="min-w-0 flex-1">
                <h1 className="font-heading text-base font-semibold text-foreground-primary">
                  {t("terminal.confirmTitle")}
                </h1>
                <p
                  className={`mt-1 font-caption text-xs leading-relaxed ${
                    error ? "text-state-danger" : "text-foreground-secondary"
                  }`}
                >
                  {error
                    ? t("terminal.backendDown", {
                        defaultValue:
                          "暂时打不开终端。若顶部有「重启」请点一下，或重新打开应用。",
                      })
                    : t("terminal.confirmDesc", {
                        workspace: activeWs?.name ?? t("common.all"),
                      })}
                </p>
              </div>
            </div>
            <Button
              className="self-start gap-1.5"
              onClick={() => {
                if (!ready || error) return;
                persistConfirmed();
                setConnPhase("connecting");
                setArmed(true);
              }}
              disabled={!ready || !!error}
            >
              <TerminalIcon className="size-3.5" />
              {error
                ? t("terminal.unavailable", { defaultValue: "后端未连接" })
                : ready
                  ? t("terminal.connect")
                  : t("common.loading")}
            </Button>
          </section>
        </div>
      )}
    </div>
  );
}
