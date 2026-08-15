//! Turn delivery — the deep seam for bootstrap / wake prompt routing.
//!
//! Plugin manifests declare [`crate::plugins::InputDelivery`] (how a CLI
//! *wants* prompts delivered). Once an agent is live, the registry slot holds
//! the concrete ports / conversation handles allocated at spawn.
//!
//! **Callers** (`wake`, bootstrap in `agent_lifecycle` / `routes::rest`) ask
//! this module to deliver a turn. They do **not** re-derive the
//! zulu → reasonix → opencode → keystroke cascade, and they do **not** call
//! engine HTTP helpers directly for wake kicks.
//!
//! [`LiveDelivery::classify`] is an implementation detail of that routing.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use crate::registry::{AgentSlot, Registry};
use crate::zulu_serve::ZuluConv;
use std::sync::Arc;

/// How a *live* agent accepts bootstrap / wake turn text.
#[derive(Clone)]
pub(crate) enum LiveDelivery {
    /// `zulu serve` per-turn SSE driver (`crate::zulu_serve`).
    Zulu(Arc<ZuluConv>),
    /// `reasonix serve` HTTP+SSE (`crate::reasonix_serve`).
    Reasonix { port: u16 },
    /// opencode TUI `/tui/*` HTTP (`crate::opencode_tui`).
    Opencode { port: u16, workspace: String },
    /// PTY keystroke paste (claude / codex / kimi).
    Keystroke,
}

impl LiveDelivery {
    /// Classify from a registry slot. Order is load-bearing: zulu before
    /// reasonix (both may set `serve_http_port`), then opencode TUI, else
    /// keystroke.
    pub(crate) fn classify(slot: &AgentSlot) -> Self {
        if let Some(conv) = slot.zulu() {
            return Self::Zulu(conv);
        }
        if let Some(port) = slot.serve_http_port() {
            return Self::Reasonix { port };
        }
        if let Some(port) = slot.tui_http_port() {
            return Self::Opencode {
                port,
                workspace: slot.workspace.clone(),
            };
        }
        Self::Keystroke
    }

    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Zulu(_) => "zulu-serve-http",
            Self::Reasonix { .. } => "reasonix-serve-http",
            Self::Opencode { .. } => "opencode-tui-http",
            Self::Keystroke => "keystroke",
        }
    }
}

/// Context for a wake-style turn (blackboard kick, manual ⚡, verify bounce).
pub struct WakeTurnCtx<'a> {
    /// This server's base URL — required for reasonix `wake_if_idle`.
    pub server_url: &'a str,
    /// When `Some`, reasonix submits this body (manual / verify). When `None`,
    /// reasonix uses the generic wake recipe after consume (auto blackboard).
    pub reasonix_body: Option<&'a str>,
}

/// Which live channel handled the wake kick (for mailbox consume policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeChannel {
    Zulu,
    Reasonix,
    Opencode,
    Keystroke,
}

/// Deliver a wake kick to a live agent through the correct channel.
///
/// **Unifies the old split:** `inject_with_kick_text` used to `Err` on
/// reasonix while `kick_agent` / `deliver_wake_with_body` had to route around
/// it. Every engine — including reasonix — is handled here.
pub async fn deliver_wake_turn(
    registry: &Registry,
    agent_id: &str,
    kick_text: &str,
    key_for_log: &str,
    ctx: WakeTurnCtx<'_>,
) -> Result<WakeChannel> {
    let slot = registry
        .get(agent_id)
        .ok_or_else(|| anyhow!("no registry slot for `{agent_id}` — agent may have exited"))?;
    // Classify under the parking_lot guard, then DROP it before any `.await`.
    let delivery = LiveDelivery::classify(&slot.lock());
    match delivery {
        LiveDelivery::Opencode { port, .. } => {
            crate::opencode_tui::deliver_turn(port, kick_text)
                .await
                .map_err(|e| anyhow!("opencode TUI wake delivery failed: {e:#}"))?;
            tracing::debug!(
                agent = %agent_id,
                key = %key_for_log,
                port,
                "wake delivered over opencode TUI HTTP"
            );
            Ok(WakeChannel::Opencode)
        }
        LiveDelivery::Zulu(conv) => {
            let submitted = crate::zulu_serve::wake_if_idle(conv, agent_id, registry).await?;
            tracing::debug!(
                agent = %agent_id,
                key = %key_for_log,
                submitted,
                "wake delivered over zulu serve HTTP"
            );
            Ok(WakeChannel::Zulu)
        }
        LiveDelivery::Reasonix { port } => {
            // Atomic consume+submit when idle; mid-turn / unreachable leave
            // the mailbox for turn_done / retry — never blind-deliver here.
            let submitted = crate::reasonix_serve::wake_if_idle(
                port,
                ctx.server_url,
                agent_id,
                ctx.reasonix_body,
            )
            .await?;
            tracing::debug!(
                agent = %agent_id,
                key = %key_for_log,
                port,
                submitted,
                "wake delivered over reasonix serve HTTP"
            );
            Ok(WakeChannel::Reasonix)
        }
        LiveDelivery::Keystroke => {
            paste_wake_keystroke(&slot, agent_id, kick_text, key_for_log).await?;
            Ok(WakeChannel::Keystroke)
        }
    }
}

async fn paste_wake_keystroke(
    slot: &Arc<parking_lot::Mutex<AgentSlot>>,
    agent_id: &str,
    kick_text: &str,
    key_for_log: &str,
) -> Result<()> {
    let input_tx = {
        let guard = slot.lock();
        match guard.pty_input() {
            Some(tx) => tx,
            None => {
                tracing::warn!(
                    agent = %agent_id,
                    key = %key_for_log,
                    "wake dropped: agent has no live PTY input"
                );
                return Ok(());
            }
        }
    };

    // Split body + delayed `\r` so Codex Ratatui does not treat the burst as
    // a paste-with-embedded-newline (M6c-7). Mirrors bootstrap keystroke path.
    let body = format!("\x15{kick_text}");
    input_tx
        .send(Bytes::from(body))
        .await
        .map_err(|e| anyhow!("PTY input_tx send (body) failed: {e}"))?;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    input_tx
        .send(Bytes::from_static(b"\r"))
        .await
        .map_err(|e| anyhow!("PTY input_tx send (submit \\r) failed: {e}"))
}

/// Outcome of the non-keystroke bootstrap branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapEngine {
    /// zulu / reasonix driver task started, or opencode first turn delivered.
    Handled,
    /// Caller must continue with PTY paste (claude / codex / kimi).
    NeedsKeystroke,
}

/// Start the engine-specific first turn (drivers / TUI HTTP). Keystroke CLIs
/// return [`BootstrapEngine::NeedsKeystroke`] so the lifecycle orchestrator
/// can apply needle waits + paste framing.
pub async fn deliver_bootstrap_engine(
    registry: &Registry,
    agent_id: &str,
    prompt: String,
    server_url: &str,
    swarm: &std::sync::Arc<swarmx_swarm::Swarm>,
) -> Result<BootstrapEngine> {
    use swarmx_protocol::ws_swarm::{AgentState, SwarmEvent};

    let slot_lock = registry
        .get(agent_id)
        .ok_or_else(|| anyhow!("slot vanished before bootstrap"))?;
    let delivery = LiveDelivery::classify(&slot_lock.lock());
    match delivery {
        LiveDelivery::Zulu(conv) => {
            crate::zulu_serve::run_driver_spawn(crate::zulu_serve::DriverCfg {
                conv,
                agent_id: agent_id.to_string(),
                bootstrap_prompt: prompt,
                registry: registry.clone(),
            });
            tracing::info!(agent = %agent_id, "bootstrap: zulu serve driver started");
            Ok(BootstrapEngine::Handled)
        }
        LiveDelivery::Reasonix { port } => {
            crate::reasonix_serve::run_driver_spawn(crate::reasonix_serve::DriverCfg {
                serve_port: port,
                agent_id: agent_id.to_string(),
                swarmx_url: server_url.to_string(),
                bootstrap_prompt: prompt,
                registry: registry.clone(),
            });
            tracing::info!(agent = %agent_id, port, "bootstrap: reasonix serve driver started");
            Ok(BootstrapEngine::Handled)
        }
        LiveDelivery::Opencode { port, workspace } => {
            match crate::opencode_tui::deliver_bootstrap(port, &prompt, &workspace).await {
                Ok(()) => {
                    tracing::info!(
                        agent = %agent_id,
                        port,
                        "bootstrap: opencode started its first turn (TUI HTTP)"
                    );
                    let at = now_ms();
                    if let Err(e) = swarm
                        .store()
                        .touch_agent_activity(agent_id.to_string(), at)
                        .await
                    {
                        tracing::debug!(
                            ?e,
                            agent = %agent_id,
                            "opencode bootstrap: touch_agent_activity failed"
                        );
                    }
                }
                Err(err) => {
                    let reason = "opencode 启动后 90s 内没能发起第一次对话（TUI 卡住，可能未登录或配置不全）"
                        .to_string();
                    let at = now_ms();
                    if let Err(e) = swarm
                        .store()
                        .record_agent_error(agent_id.to_string(), reason.clone(), "fatal", at)
                        .await
                    {
                        tracing::warn!(
                            ?e,
                            agent = %agent_id,
                            "opencode bootstrap: record_agent_error failed"
                        );
                    }
                    tracing::warn!(
                        agent = %agent_id,
                        port,
                        ?err,
                        "bootstrap: opencode never started a turn (TUI HTTP) — flipped to Error"
                    );
                    swarm.publish_event(SwarmEvent::AgentState {
                        agent_id: agent_id.to_string(),
                        state: AgentState::Error,
                    });
                    swarm.publish_event(SwarmEvent::AgentActivity {
                        agent_id: agent_id.to_string(),
                        kind: "system".to_string(),
                        label: reason,
                        phase: "error".to_string(),
                        seq: 0,
                        duration_ms: None,
                        at,
                    });
                }
            }
            Ok(BootstrapEngine::Handled)
        }
        LiveDelivery::Keystroke => Ok(BootstrapEngine::NeedsKeystroke),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty_stream::PtyStream;
    use crate::registry::{AgentChannel, Lifecycle};
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::mpsc;

    fn bare_slot() -> AgentSlot {
        let (input_tx, _rx) = mpsc::channel::<Bytes>(4);
        let (lifecycle_tx, _) = tokio::sync::broadcast::channel(4);
        AgentSlot {
            channel: AgentChannel::Pty {
                bridge: {
                    use swarmx_pty::{PtyBridge, SpawnOpts};
                    use std::collections::HashMap;
                    let handles = PtyBridge::spawn(SpawnOpts {
                        argv: &["/bin/sh".into(), "-c".into(), "true".into()],
                        cwd: None,
                        env: HashMap::new(),
                        cols: 40,
                        rows: 12,
                    })
                    .expect("spawn classification stub");
                    Arc::new(handles.bridge)
                },
                stream: Arc::new(PtyStream::new()),
                input_tx,
            },
            lifecycle: Arc::new(Mutex::new(Lifecycle::default())),
            lifecycle_tx,
            cli: "test".into(),
            role: "test".into(),
            workspace: "/tmp/ws".into(),
            paused: Arc::new(AtomicBool::new(false)),
            mcp_ready: tokio::sync::watch::channel(false).0,
            tui_http_port: None,
            serve_http_port: None,
            zulu: None,
        }
    }

    #[test]
    #[cfg(unix)]
    fn classify_defaults_to_keystroke() {
        let slot = bare_slot();
        assert!(matches!(LiveDelivery::classify(&slot), LiveDelivery::Keystroke));
    }

    #[test]
    #[cfg(unix)]
    fn classify_opencode_from_tui_port() {
        let mut slot = bare_slot();
        slot.tui_http_port = Some(4096);
        match LiveDelivery::classify(&slot) {
            LiveDelivery::Opencode { port, workspace } => {
                assert_eq!(port, 4096);
                assert_eq!(workspace, "/tmp/ws");
            }
            other => panic!("expected Opencode, got {}", other.kind_name()),
        }
    }

    #[test]
    #[cfg(unix)]
    fn classify_reasonix_from_serve_port() {
        let mut slot = bare_slot();
        slot.serve_http_port = Some(7780);
        match LiveDelivery::classify(&slot) {
            LiveDelivery::Reasonix { port } => assert_eq!(port, 7780),
            other => panic!("expected Reasonix, got {}", other.kind_name()),
        }
    }

    #[test]
    #[cfg(unix)]
    fn classify_zulu_wins_over_serve_port() {
        let mut slot = bare_slot();
        slot.serve_http_port = Some(7780);
        slot.zulu = Some(Arc::new(crate::zulu_serve::ZuluConv::new(
            7780,
            "m".into(),
            "l".into(),
            "/tmp".into(),
            "http://127.0.0.1:7777".into(),
        )));
        assert!(matches!(
            LiveDelivery::classify(&slot),
            LiveDelivery::Zulu(_)
        ));
    }
}
