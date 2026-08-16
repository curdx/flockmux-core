//! Per-agent registry: holds the live `PtyBridge`s keyed by agent id.
//!
//! Each agent owns:
//!  - a `PtyBridge` (the PTY + child),
//!  - a shared `PtyStream` ring buffer (M3): the PTY-reader background task
//!    appends every chunk here; one or more WS subscribers each hold their
//!    own cursor and replay/follow from the buffer. Replaces the M1
//!    single-consumer `output_rx` claim.
//!  - a clone of the input `Sender` (to inject keystrokes).
//!
//! Storage is a `DashMap<AgentId, Arc<Mutex<AgentSlot>>>` so HTTP and WS
//! handlers can claim/release independently without serialising the whole
//! map.

use crate::pty_stream::PtyStream;
use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use swarmx_pty::PtyBridge;
use tokio::sync::mpsc;

/// Lifecycle status surfaced to every (re-)attaching client so the UI can
/// render the current state without waiting for the next OSC event. Mutated
/// by the per-agent pump task whenever the OSC scanner detects a marker.
#[derive(Debug, Default, Clone, Copy)]
pub struct Lifecycle {
    pub shim_ready: bool,
    pub shim_exit: Option<i32>,
}

pub struct AgentSlot {
    /// The PTY carrying this agent's I/O — every CLI (claude/codex/opencode/
    /// reasonix/zulu/kimi) runs as an interactive TUI scraped over a
    /// pseudo-terminal. Consumers go
    /// through the `AgentSlot` methods (`is_alive`, `pty_stream`, `pty_input`,
    /// …) rather than touching the channel directly.
    pub channel: AgentChannel,
    /// Shim lifecycle state captured by the pump's OSC scanner. Stored
    /// here so a fresh WS attach can be told ShimReady/ShimExit even if
    /// the OSC marker fired before the client connected.
    pub lifecycle: Arc<Mutex<Lifecycle>>,
    /// Broadcasts ShimReady/ShimExit so every attached WS writer relays
    /// the event live (in addition to the snapshot delivered via Hello).
    /// `subscribe()` is cheap and lossless for the small lifecycle volume.
    pub lifecycle_tx: tokio::sync::broadcast::Sender<LifecycleEvent>,
    pub cli: String,
    pub role: String,
    pub workspace: String,
    /// In-memory pause flag. When true, the WakeCoordinator's auto-wake
    /// path (BlackboardChanged → deliver_wake) short-circuits and does not
    /// inject PTY bytes for this agent. Manual operator wakes bypass this.
    /// Persisted lifetime = process lifetime; resets on server restart.
    pub paused: Arc<AtomicBool>,
    /// MCP-readiness gate. The agent's `swarmx-mcp` subprocess flips this to
    /// `true` (via `POST /api/agent/:id/mcp-ready`) the moment the CLI fetches
    /// its tool list — per the MCP lifecycle that means the model can now see
    /// the `swarm_*` tools. The bootstrap waits on this (with a fallback
    /// timeout) instead of a fixed sleep: the readiness-probe pattern. `watch`
    /// retains the latest value, so a ping that races the subscriber is never
    /// lost (`send_replace` updates the stored value even with no receivers).
    pub mcp_ready: tokio::sync::watch::Sender<bool>,
    /// Allocated opencode `--port`. Handle only — the live channel is
    /// [`Self::live_delivery`]. `None` when this agent is not opencode.
    pub tui_http_port: Option<u16>,
    /// Allocated `reasonix serve` / `zulu serve` listen port. Handle only —
    /// those two engines share this field; do **not** infer the channel from
    /// it. Read [`Self::live_delivery`].
    pub serve_http_port: Option<u16>,
    /// Allocated zulu conversation handle. Handle only — the live channel is
    /// [`Self::live_delivery`].
    pub zulu: Option<Arc<crate::zulu_serve::ZuluConv>>,
    /// Live turn-delivery channel, set once at spawn from the plugin's
    /// declared `InputDelivery` plus the handles above. Callers read this;
    /// they do not re-derive from the Options.
    pub(crate) live_delivery: crate::input_delivery::LiveDelivery,
}

/// An agent's PTY I/O. Every CLI (claude/codex/opencode/reasonix/zulu/kimi)
/// runs as an interactive TUI scraped over a pseudo-terminal. Kept as a
/// single-variant enum so the
/// accessor methods below give consumers a stable, channel-agnostic surface.
pub enum AgentChannel {
    Pty {
        bridge: Arc<PtyBridge>,
        /// Shared resume buffer fed by the PTY-reader pump and observed by
        /// every WS subscriber. Survives WS disconnect/reconnect.
        stream: Arc<PtyStream>,
        /// Inject keystrokes into the PTY (wake "kicks", terminal WS input).
        input_tx: mpsc::Sender<Bytes>,
    },
}

impl AgentSlot {
    /// Is the agent's underlying process still running?
    pub fn is_alive(&self) -> bool {
        match &self.channel {
            AgentChannel::Pty { bridge, .. } => bridge.is_alive(),
        }
    }

    /// The process exit code if it has already exited, else `None`.
    pub fn try_exit_code(&self) -> Option<i32> {
        match &self.channel {
            AgentChannel::Pty { bridge, .. } => bridge.try_exit_code(),
        }
    }

    /// The shim's OS pid — also its process-group id on unix, since
    /// portable-pty `setsid`s the shim at spawn, making it the group leader of
    /// the whole CLI tree. The reaper persists this into the agents row
    /// (`record_agent_shim_pid`) so a server crash/SIGKILL leaves enough of a
    /// process ledger for the next boot to really kill the orphaned tree
    /// instead of just marking the row killed.
    pub fn shim_pid(&self) -> Option<u32> {
        match &self.channel {
            AgentChannel::Pty { bridge, .. } => bridge.pid(),
        }
    }

    /// Terminate the agent's underlying process.
    pub fn kill(&self) {
        match &self.channel {
            AgentChannel::Pty { bridge, .. } => bridge.kill(),
        }
    }

    /// The PTY bridge (terminal WS attach).
    pub fn pty_bridge(&self) -> Option<Arc<PtyBridge>> {
        match &self.channel {
            AgentChannel::Pty { bridge, .. } => Some(bridge.clone()),
        }
    }

    /// The PTY output ring buffer (what the terminal WS endpoints serve).
    pub fn pty_stream(&self) -> Option<Arc<PtyStream>> {
        match &self.channel {
            AgentChannel::Pty { stream, .. } => Some(stream.clone()),
        }
    }

    /// The PTY stdin sender for byte injection (wake kick / terminal input).
    pub fn pty_input(&self) -> Option<mpsc::Sender<Bytes>> {
        match &self.channel {
            AgentChannel::Pty { input_tx, .. } => Some(input_tx.clone()),
        }
    }

    /// The TUI HTTP-control port for opencode agents (see `tui_http_port` field
    /// and `crate::opencode_tui`). Handle only — channel is [`Self::live_delivery`].
    pub fn tui_http_port(&self) -> Option<u16> {
        self.tui_http_port
    }

    /// The `reasonix serve` / `zulu serve` listen port. Handle only — those
    /// two engines share this field. Channel is [`Self::live_delivery`].
    pub fn serve_http_port(&self) -> Option<u16> {
        self.serve_http_port
    }

    /// The per-agent zulu conversation handle (see `zulu` field and
    /// `crate::zulu_serve`). Handle only — channel is [`Self::live_delivery`].
    pub fn zulu(&self) -> Option<Arc<crate::zulu_serve::ZuluConv>> {
        self.zulu.clone()
    }

    pub(crate) fn live_delivery(&self) -> crate::input_delivery::LiveDelivery {
        self.live_delivery.clone()
    }
}

#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    ShimReady,
    ShimExit(i32),
    /// The CLI is alive but printed a banner meaning it *cannot actually do
    /// work* — the canonical case is claude's "Not logged in · Run /login"
    /// when no OAuth credential is present, or a rate-limit/quota notice. The
    /// PTY pump's `HealthScanner` raises this on the first match so the
    /// lifecycle subscriber can flip the agent to `AgentState::Error` and ride
    /// the human-facing detail on a system `AgentActivity` — replacing the fake
    /// "online" green dot + "暂无消息" with an honest, actionable failure card.
    ///
    /// Carries a `String` payload, so this enum is `Clone` (not `Copy`); the
    /// broadcast channel only requires `Clone`.
    HealthFail {
        reason: String,
        kind: String,
    },
}

#[derive(Default, Clone)]
pub struct Registry {
    inner: Arc<DashMap<String, Arc<Mutex<AgentSlot>>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, agent_id: String, slot: AgentSlot) {
        self.inner.insert(agent_id, Arc::new(Mutex::new(slot)));
    }

    pub fn get(&self, agent_id: &str) -> Option<Arc<Mutex<AgentSlot>>> {
        self.inner.get(agent_id).map(|e| e.clone())
    }

    pub fn remove(&self, agent_id: &str) -> Option<Arc<Mutex<AgentSlot>>> {
        let slot = self.inner.remove(agent_id).map(|(_, v)| v);
        if slot.is_some() {
            // Slot removal is THE convergence point for agent retirement —
            // REST teardown, the wake coordinator's auto-kill, and the
            // reaper's post-grace eviction all funnel through here — so this
            // is where the per-agent config scratch under `~/.swarmx/`
            // (codex/reasonix homes, opencode/mcp/wake entries) gets
            // reclaimed. A handful of small removes, idempotent, and
            // path-validated inside; previously only the engine probe ever
            // cleaned these, so torn-down agents leaked them forever.
            crate::engine_probe::remove_agent_scratch(agent_id);
        }
        slot
    }

    /// Snapshot of every live agent. Used by `GET /api/agent` so a freshly
    /// loaded UI can reattach to agents spawned before this tab existed.
    pub fn list(&self) -> Vec<(String, Arc<Mutex<AgentSlot>>)> {
        self.inner
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }
}

/// Terminate an agent's PTY **without stalling the async runtime**.
///
/// `PtyBridge::kill()` blocks the calling OS thread for up to ~1s in its
/// SIGTERM→grace→SIGKILL loop. Calling it inline on a tokio worker steals that
/// worker for the whole second (it can't poll other tasks); calling it while
/// holding the slot `Mutex` additionally freezes every concurrent `slot.lock()`
/// (the pty_ws writer's lifecycle snapshot, Resize, the reaper's detect_exit).
/// Under a fan-out round where N workers auto-kill at once, that stalls the
/// entire server. So: clone the bridge `Arc` out from under the lock, drop the
/// guard, then offload the blocking kill to the blocking pool. Only
/// `PtyBridge::Drop` (a sync context) may call `kill()` directly.
pub async fn offload_kill(slot: &Arc<Mutex<AgentSlot>>) {
    let bridge = {
        let s = slot.lock();
        s.pty_bridge()
    };
    if let Some(bridge) = bridge {
        let _ = tokio::task::spawn_blocking(move || bridge.kill()).await;
    }
}
