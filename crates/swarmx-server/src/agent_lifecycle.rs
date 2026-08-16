//! Agent lifecycle orchestration: ShimReady → mcp-ready → readiness gate →
//! first-turn delivery (engine drivers or PTY paste).
//!
//! Extracted from `routes::rest` so HTTP handlers stay thin and the timing-
//! sensitive bootstrap sequence has one home (F22). Callers:
//! `spawn_worker` / `run_spell` / workspace fusion helpers.

use crate::registry::LifecycleEvent;
use swarmx_protocol::ws_swarm::SwarmEvent;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Readiness-gate watchdog shield (unit-tested): a dep-gated worker waits
/// silently BY DESIGN — its first prompt isn't injected until the deps land —
/// yet the one-shot first-response watchdog (90s/150s after ShimReady) probes
/// `agent_silent_since_ready` and, once the wait outlasts that probe, misflags
/// the healthy waiter as wedged. The gate therefore stamps
/// `touch_agent_activity` while the wait is this long. Shorter waits must NOT
/// stamp: `last_activity_at` is permanent once set, and a worker that clears
/// the gate quickly and THEN truly wedges before any sign of life has to stay
/// catchable by the watchdog. 60s sits comfortably below the 90s minimum
/// window (the gate starts ≤6s after ShimReady, post MCP-settle).
pub(crate) fn gate_should_shield_watchdog(elapsed: std::time::Duration) -> bool {
    elapsed >= std::time::Duration::from_secs(60)
}

/// First dependency not yet satisfied — neither the key itself NOR its
/// `.error`/`.failed` failure alias is present on the blackboard — or `None` if
/// all are satisfied. Pure (unit-tested); drives the P1-D readiness gate's
/// "are this worker's inputs ready?" decision. A `.error`/`.failed` alias counts
/// as satisfied so a downstream worker wakes to handle an upstream FAILURE
/// rather than waiting forever for a key the dead producer will never write.
pub(crate) fn first_unsatisfied_dep(
    deps: &[String],
    present: &std::collections::HashSet<String>,
) -> Option<String> {
    deps.iter()
        .find(|k| {
            !present.contains(k.as_str())
                && !present.contains(format!("{k}.error").as_str())
                && !present.contains(format!("{k}.failed").as_str())
        })
        .cloned()
}

/// Per-agent bootstrap-injection context — the only things that differ
/// between the `spawn_worker` (ad-hoc) and `run_spell` launch paths.
pub(crate) struct BootstrapCtx {
    /// "worker" or "spell" — surfaced in log lines.
    pub(crate) source: &'static str,
    /// Spell name for spell-launched agents; empty for ad-hoc workers.
    pub(crate) spell: String,
    /// Declared role-id keys; used to flag a surviving `{<role>_id}` / `{task}`
    /// placeholder in the rendered prompt (empty for raw worker prompts).
    pub(crate) role_keys: Vec<String>,
}

/// Background task: wait for `ShimReady` (short-circuit if it already fired),
/// let the agent's MCP servers settle, then paste `prompt` + Enter into its
/// PTY. Fail-soft — every error path `warn!`s and returns.
///
/// This is the SINGLE home of the timing-sensitive bootstrap sequence. It was
/// previously copy-pasted between `spawn_worker` and `run_spell` (the F22
/// finding); extracting it means the 2500ms MCP-settle window, the
/// paste→150ms→`\r` submit split, and the ShimReady race handling can never
/// drift between the two paths.
pub(crate) fn spawn_bootstrap_inject(
    registry: crate::registry::Registry,
    mut rx: tokio::sync::broadcast::Receiver<LifecycleEvent>,
    agent_id: String,
    prompt: String,
    ctx: BootstrapCtx,
    // P1-D readiness gate: blackboard keys this agent depends on. The first
    // prompt is NOT injected until all are present (or their `.error`/`.failed`
    // alias). Empty ⇒ inject immediately (orchestrators / dep-less workers).
    deps: Vec<String>,
    swarm: std::sync::Arc<swarmx_swarm::Swarm>,
    // The plugin catalog — used to read the agent's keystroke framing flags
    // (e.g. kimi's `bracketed_paste`) off the slot's plugin id.
    plugins: std::sync::Arc<crate::plugins::PluginRegistry>,
    // This worker's spawn time (unix-ms). A dep only satisfies the gate if its
    // latest blackboard write is at/after this — so a STALE key left on disk by
    // a PRIOR run on the same thread can't bypass the gate.
    spawned_at: i64,
    // This server's own base URL (loopback). Threaded to the reasonix SSE driver
    // so it can reach consume_wakes + the activity ingress; unused by the
    // keystroke / opencode paths.
    server_url: String,
) {
    tokio::spawn(async move {
        // Short-circuit if ShimReady already fired in the gap between
        // spawn_agent returning and our resubscribe — the PTY pump runs
        // concurrently with the spawn caller, so for fast CLIs OSC_READY can
        // arrive before a receiver is hooked up. Reading the mutex covers it.
        let already_ready = registry
            .get(&agent_id)
            .map(|s| s.lock().lifecycle.lock().shim_ready)
            .unwrap_or(false);
        if !already_ready {
            let wait_ready = async {
                loop {
                    match rx.recv().await {
                        Ok(LifecycleEvent::ShimReady) => return Ok(()),
                        Ok(LifecycleEvent::ShimExit(code)) => {
                            return Err(format!("agent exited before ShimReady (code={code})"));
                        }
                        // Auth/quota failure is reported independently (the
                        // lifecycle subscriber publishes Error); keep waiting for
                        // ShimReady so injection still follows the normal path.
                        Ok(LifecycleEvent::HealthFail { .. }) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err("lifecycle channel closed".into());
                        }
                    }
                }
            };
            match tokio::time::timeout(std::time::Duration::from_secs(30), wait_ready).await {
                Ok(Ok(())) => {}
                Ok(Err(msg)) => {
                    tracing::warn!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, msg = %msg, "bootstrap aborted");
                    return;
                }
                Err(_) => {
                    tracing::warn!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, "bootstrap timed out waiting for ShimReady");
                    return;
                }
            }
        }
        // Wait until the agent's MCP tools are actually visible to the model
        // before injecting — otherwise the model reads an empty toolset and
        // hand-waves "I don't have a swarm_send_message tool". The agent's own
        // swarmx-mcp pings /api/agent/:id/mcp-ready when the CLI fetches its
        // tool list (MCP lifecycle), flipping the slot's `mcp_ready` watch. We
        // wait for that real signal (readiness-probe pattern) with a bounded
        // fallback for any CLI/case that never pings. This replaces a fixed
        // 2500ms sleep: claude/codex emit no stable "MCP ready" banner to
        // scrape (verified empirically), and a fixed sleep both over-waits on
        // fast starts and under-waits on slow ones (a known anti-pattern).
        let slot_lock = match registry.get(&agent_id) {
            Some(s) => s,
            None => {
                tracing::warn!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, "slot vanished before bootstrap");
                return;
            }
        };
        // HTTP serve engines (reasonix documented; zulu same class) connect MCP
        // clients only AFTER the first submit, so the mcp-ready ping can never
        // arrive before we submit — waiting here would just burn the full
        // fallback every spawn. Keyed off stored LiveDelivery, not
        // `serve_http_port` (zulu and reasonix share that port field).
        let skip_mcp_ready = slot_lock.lock().live_delivery().skips_mcp_ready_wait();
        // Subscribe without holding the parking_lot guard across the await.
        let mut mcp_rx = slot_lock.lock().mcp_ready.subscribe();
        if !skip_mcp_ready && !*mcp_rx.borrow() {
            // Generous cap: only applies when the ping never arrives (e.g. a
            // future CLI without MCP, or a lost ping). On the happy path the
            // watch fires in ~1-2s and we proceed immediately.
            const MCP_READY_FALLBACK: std::time::Duration = std::time::Duration::from_secs(6);
            tokio::select! {
                _ = mcp_rx.changed() => {
                    tracing::debug!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, "mcp ready; injecting bootstrap");
                }
                _ = tokio::time::sleep(MCP_READY_FALLBACK) => {
                    tracing::warn!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, "mcp-ready not signalled within fallback; injecting anyway");
                }
            }
        }

        // ── P1-D readiness gate ───────────────────────────────────────────
        // Do NOT inject the worker's first prompt until every declared
        // dependency (or its `.error`/`.failed` failure alias) is on the
        // blackboard. A dependent worker therefore CANNOT run its first turn on
        // inputs that don't exist yet — the premature-execution bug (observed:
        // a reviewer judged FAIL before its producer wrote the file) is made
        // structurally impossible at the mechanism level; the prompt INPUTS
        // block becomes a secondary catch. The PTY sits idle (no tokens) while
        // waiting; the producer's write lands the key and the next poll
        // proceeds. A producer that DIES writes `<key>.error` (M6c), accepted by
        // the alias check so the worker wakes to handle the failure rather than
        // hang. Aborts if the agent is killed meanwhile.
        if !deps.is_empty() {
            const POLL: std::time::Duration = std::time::Duration::from_millis(750);
            const LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
            // Bound: if a declared producer is NEVER spawned (and so never writes
            // a key OR a `.error`), don't poll forever as a phantom-alive agent.
            // On timeout, inject anyway — the prompt INPUTS block then catches the
            // missing input and the worker fails LOUD (surfacing the mistake to
            // the orchestrator) instead of hanging invisibly.
            const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(300);
            let start = std::time::Instant::now();
            let mut since_log = LOG_EVERY; // log once immediately on first wait
            loop {
                if registry.get(&agent_id).is_none() {
                    tracing::info!(agent = %agent_id, "readiness gate: agent gone before deps satisfied; aborting bootstrap");
                    return;
                }
                // A dep counts as present only if its latest blackboard write is
                // FRESH (`at >= spawned_at`). A stale key left by a prior run on
                // the same thread must NOT satisfy the gate — else the premature-
                // execution bug silently returns against stale inputs. `.error`/
                // `.failed` aliases count (fail-loud on producer death).
                let mut present = std::collections::HashSet::new();
                for key in &deps {
                    for probe in [key.clone(), format!("{key}.error"), format!("{key}.failed")] {
                        let fresh = swarm
                            .store()
                            .list_blackboard_ops(Some(probe.clone()))
                            .await
                            .ok()
                            .and_then(|ops| ops.first().map(|r| r.at))
                            .is_some_and(|at| at >= spawned_at);
                        if fresh {
                            present.insert(probe);
                        }
                    }
                }
                let missing = first_unsatisfied_dep(&deps, &present);
                if missing.is_none() {
                    tracing::info!(agent = %agent_id, deps = ?deps, "readiness gate: deps satisfied; injecting first turn");
                    break;
                }
                if start.elapsed() >= MAX_WAIT {
                    tracing::warn!(agent = %agent_id, waiting_for = ?missing, max_wait_s = MAX_WAIT.as_secs(), "readiness gate: timed out; injecting anyway (producer may never have spawned) — worker's INPUTS block will fail loud");
                    break;
                }
                if since_log >= LOG_EVERY {
                    tracing::info!(agent = %agent_id, waiting_for = ?missing, deps = ?deps, elapsed_s = start.elapsed().as_secs(), "readiness gate: holding first turn until deps land");
                    since_log = std::time::Duration::ZERO;
                }
                // First-response watchdog shield: this silent wait is legitimate
                // (no prompt injected yet), but the watchdog probes once at
                // 90s/150s after ShimReady and would flip a healthy waiter to
                // Error — wake.rs then routes it into handle_agent_exit, writes
                // `<handoff>.error` and wakes downstream down the "upstream
                // failed" branch while the producer is fine. Stamp activity so
                // agent_silent_since_ready stays false while the gate holds.
                if gate_should_shield_watchdog(start.elapsed()) {
                    if let Err(e) = swarm
                        .store()
                        .touch_agent_activity(agent_id.clone(), now_ms())
                        .await
                    {
                        tracing::debug!(?e, agent = %agent_id, "readiness gate: watchdog shield touch failed");
                    }
                }
                tokio::time::sleep(POLL).await;
                since_log += POLL;
            }
        }

        // Engine-specific first turn (zulu/reasonix drivers, opencode TUI).
        // Keystroke CLIs fall through to needle wait + PTY paste below.
        match crate::input_delivery::deliver_bootstrap_engine(
            &registry,
            &agent_id,
            prompt.clone(),
            &server_url,
            &swarm,
        )
        .await
        {
            Ok(crate::input_delivery::BootstrapEngine::Handled) => return,
            Ok(crate::input_delivery::BootstrapEngine::NeedsKeystroke) => {}
            Err(err) => {
                tracing::warn!(
                    source = ctx.source,
                    spell = %ctx.spell,
                    agent = %agent_id,
                    ?err,
                    "bootstrap engine delivery failed"
                );
                return;
            }
        }
        // kimi-class readiness gate: wait for the TUI's OWN settled banner
        // (`bootstrap_ready_needle` in the manifest) before pasting. kimi's
        // mcp-ready ping fires early (its tool fetch precedes the input
        // pipeline becoming stable) — a paste landing in that window is
        // silently eaten (ctx stays 0%; measured 2/4 spawns). Bounded: a
        // missing banner injects anyway and the first-response watchdog
        // judges the outcome. Empty needle (claude/codex) skips the gate.
        let (ready_needle, ready_settle_ms) = {
            let cli = slot_lock.lock().cli.clone();
            match plugins.get(&cli) {
                Some(p) => (
                    p.bootstrap_ready_needle.clone(),
                    p.bootstrap_ready_settle_ms,
                ),
                None => (String::new(), 0),
            }
        };
        if !ready_needle.is_empty() {
            let stream = slot_lock.lock().pty_stream();
            let found = match stream {
                Some(s) => {
                    wait_for_pty_needle(&s, ready_needle.as_bytes(), BOOTSTRAP_READY_NEEDLE_TIMEOUT)
                        .await
                }
                None => false,
            };
            if found {
                if ready_settle_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(ready_settle_ms)).await;
                }
                tracing::debug!(source = ctx.source, agent = %agent_id, needle = %ready_needle, "bootstrap readiness needle seen; pasting");
            } else {
                tracing::warn!(source = ctx.source, agent = %agent_id, needle = %ready_needle, "bootstrap readiness needle not seen in time; injecting anyway");
            }
        }
        let pty_input = slot_lock.lock().pty_input();
        let Some(input_tx) = pty_input else {
            tracing::warn!(agent = %agent_id, "bootstrap: agent has no live PTY input; first turn not delivered");
            return;
        };
        // SECURITY: strip ANSI / terminal-control bytes before they hit the PTY.
        // The prompt is machine-rendered from spell/role/worker text that may carry
        // ESC/CSI/OSC sequences or other control chars; injected verbatim they let
        // the source manipulate the agent's TUI and the user's terminal (incl.
        // INVISIBLE prompt injection that hides what the model was told). Keeps
        // visible text + `\n`/`\t`; drops `\r` (would prematurely submit the paste)
        // and all other control codes. See `spells::sanitize_pty_inject`.
        let prompt = crate::spells::sanitize_pty_inject(&prompt);
        // Diagnostic: flag a surviving `{task}` / `{<role>_id}` placeholder
        // (computed before `prompt` is consumed by `into_bytes`).
        let has_unsubst = prompt.contains("{task}")
            || ctx
                .role_keys
                .iter()
                .any(|r| prompt.contains(&format!("{{{r}_id}}")));
        let body = prompt.into_bytes();
        let body_len = body.len();
        // kimi declares `bracketed_paste`: wrap the body in explicit
        // `ESC[200~`…`ESC[201~` markers so its TUI treats the (large) paste as
        // ONE atomic paste — without them the trailing `\r` can be absorbed
        // as a newline mid-burst and the turn never starts (live-verified).
        // Resolved off the slot's plugin id; claude/codex keep the raw-burst
        // framing they're proven on. The settle scaling below still keys off
        // the PROMPT length (markers add a constant 12 bytes).
        let bracketed = {
            let cli = slot_lock.lock().cli.clone();
            plugins
                .get(&cli)
                .map(|p| p.bracketed_paste)
                .unwrap_or(false)
        };
        let body = if bracketed {
            let mut b = Vec::with_capacity(body.len() + 12);
            b.extend_from_slice(b"\x1b[200~");
            b.extend_from_slice(&body);
            b.extend_from_slice(b"\x1b[201~");
            b
        } else {
            body
        };
        // Submit as separate frames (paste body, settle, then \r): claude/
        // codex TUIs classify a burst containing newlines as a *paste*, so a
        // \r in the same burst becomes a literal newline rather than a submit.
        // Splitting lets the TUI settle the paste, then the standalone \r reads
        // as Enter.
        //
        // The settle delay MUST scale with prompt size. A cold-start TUI takes
        // longer to drain + classify a large bracketed paste; a \r that lands
        // before the paste closes is swallowed into the paste buffer and never
        // submits. Observed in QA: a 21988-byte `init` orchestrator prompt left
        // claude parked at Ctx:0 forever (green "READY", no greeting) — a manual
        // Enter unstuck it instantly. A flat 150ms is only safe for small
        // prompts. We scale ~1ms per 100 bytes on top of a 150ms floor, and then
        // re-send \r once more after a further gap as a safety net: if the first
        // \r was absorbed by a still-open paste, the second (well after the paste
        // has closed) submits; if the first already submitted, the second lands
        // on an empty prompt and is a harmless no-op.
        if let Err(err) = input_tx.send(bytes::Bytes::from(body)).await {
            tracing::warn!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, ?err, "PTY paste send failed during bootstrap");
            return;
        }
        let settle_ms = 150 + (body_len as u64 / 100);
        tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
        if let Err(err) = input_tx.send(bytes::Bytes::from_static(b"\r")).await {
            tracing::warn!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, ?err, "PTY submit send failed during bootstrap");
            return;
        }
        // Safety net: re-submit once after the paste has certainly closed. A
        // second Enter on an already-submitted (now empty) prompt is a no-op.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        if let Err(err) = input_tx.send(bytes::Bytes::from_static(b"\r")).await {
            tracing::warn!(source = ctx.source, spell = %ctx.spell, agent = %agent_id, ?err, "PTY re-submit send failed during bootstrap");
        }
        tracing::info!(
            source = ctx.source,
            spell = %ctx.spell,
            agent = %agent_id,
            bytes = body_len,
            has_unsubstituted_placeholders = has_unsubst,
            "bootstrap prompt injected"
        );
        // Stage heartbeat for the cold-start progress UI: the first prompt is
        // now submitted; the "first turn" boundary is the first AgentActivity
        // that follows (no extra event needed).
        swarm.publish_event(SwarmEvent::AgentStage {
            agent_id: agent_id.clone(),
            stage: "bootstrap_injected".into(),
            at: now_ms(),
        });
    });
}

/// How long the keystroke bootstrap waits for a plugin's
/// `bootstrap_ready_needle` before giving up and pasting anyway. kimi's
/// "MCP server … connected" banner lands <2s after spawn on a warm box; 45s
/// covers a cold first run (plugin/theme init) while still leaving the 90s
/// first-response watchdog room to judge a genuinely wedged agent.
const BOOTSTRAP_READY_NEEDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Scan an agent's PTY output for `needle` — ring buffer first, then live
/// appends — returning `true` once found (`false` on timeout or stream
/// close). Plain byte search over a rolling stitch window (needles are short
/// ASCII banners; no decoding). Used by the keystroke bootstrap's
/// kimi-class readiness gate: the TUI's own banner is the only trustworthy
/// "input pipeline is stable" signal we have.
pub(crate) async fn wait_for_pty_needle(
    stream: &std::sync::Arc<crate::pty_stream::PtyStream>,
    needle: &[u8],
    timeout: std::time::Duration,
) -> bool {
    use crate::pty_stream::FetchResult;
    if needle.is_empty() {
        return true;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut cursor: u32 = 0; // 0 = replay from the oldest still-buffered entry
    let mut window: Vec<u8> = Vec::new();
    /// Stitch across chunk boundaries without unbounded growth (a banner is
    /// <100 bytes; 64KB of tail is far more than enough).
    const WINDOW_CAP: usize = 64 * 1024;
    loop {
        match stream.fetch_since(cursor) {
            FetchResult::Ok(entries) => {
                for (seq, bytes) in entries {
                    cursor = seq;
                    window.extend_from_slice(&bytes);
                }
                if window.len() > WINDOW_CAP {
                    let drop = window.len() - WINDOW_CAP;
                    window.drain(..drop);
                }
                if window.windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
            }
            FetchResult::Gap { current_seq } => {
                // Buffer wrapped past us — resync and keep watching.
                cursor = current_seq;
                window.clear();
            }
        }
        if stream.snapshot().closed {
            return false;
        }
        tokio::select! {
            _ = stream.wait_changed(cursor) => {}
            _ = tokio::time::sleep_until(deadline) => return false,
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
    }
}

// ── Orchestrator singleton (zero-burden captain) ─────────────────────────
// Magentic-One / init.md: one point of contact per direction. Workers already
// reject duplicate live producers; orchestrators used to slip through and leave
// silent twins burning subscription quota. Pure helpers below drive both the
// spawn idempotency gate and the reaper's duplicate teardown.

/// Live-orchestrator snapshot used to pick a keeper among duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchCandidate {
    pub id: String,
    pub spawned_at: i64,
    pub last_activity_at: Option<i64>,
    pub last_error_kind: Option<String>,
}

/// Higher score = better keeper. Prefer anyone who has done work, then anyone
/// without a soft error, then the older spawn (stable identity).
fn orch_keeper_score(c: &OrchCandidate) -> (i32, i64) {
    let life = if c.last_activity_at.is_some() {
        2
    } else if c.last_error_kind.as_deref().is_none() {
        1
    } else {
        0
    };
    (life, -c.spawned_at)
}

/// The single captain we keep when several are live. `None` only if `cands` empty.
pub fn pick_orchestrator_keeper(cands: &[OrchCandidate]) -> Option<&OrchCandidate> {
    cands.iter().max_by_key(|c| orch_keeper_score(c))
}

/// Agent ids that must be torn down so exactly one captain remains.
/// Empty when there is already ≤1 live orchestrator — the sole captain's
/// soft watchdog error is NEVER auto-killed (user may still `/login`).
pub fn duplicate_orchestrators_to_reap(cands: &[OrchCandidate]) -> Vec<String> {
    if cands.len() <= 1 {
        return Vec::new();
    }
    let keeper_id = pick_orchestrator_keeper(cands).map(|c| c.id.as_str());
    cands
        .iter()
        .filter(|c| Some(c.id.as_str()) != keeper_id)
        .map(|c| c.id.clone())
        .collect()
}

#[cfg(test)]
mod orch_singleton_tests {
    use super::*;

    fn c(id: &str, spawned_at: i64, activity: Option<i64>, err: Option<&str>) -> OrchCandidate {
        OrchCandidate {
            id: id.into(),
            spawned_at,
            last_activity_at: activity,
            last_error_kind: err.map(str::to_string),
        }
    }

    #[test]
    fn single_orch_never_reaped_even_with_watchdog() {
        let only = vec![c("a", 1, None, Some("watchdog"))];
        assert!(duplicate_orchestrators_to_reap(&only).is_empty());
        assert_eq!(pick_orchestrator_keeper(&only).unwrap().id, "a");
    }

    #[test]
    fn prefers_active_captain_over_silent_watchdog_twin() {
        let cands = vec![
            c("silent", 100, None, Some("watchdog")),
            c("active", 200, Some(999), None),
        ];
        assert_eq!(pick_orchestrator_keeper(&cands).unwrap().id, "active");
        assert_eq!(
            duplicate_orchestrators_to_reap(&cands),
            vec!["silent".to_string()]
        );
    }

    #[test]
    fn prefers_healthy_silent_over_watchdog_when_neither_has_activity() {
        // Both idle: keep the one without soft error (may still be booting).
        let cands = vec![
            c("wedged", 100, None, Some("watchdog")),
            c("fresh", 200, None, None),
        ];
        assert_eq!(pick_orchestrator_keeper(&cands).unwrap().id, "fresh");
        assert_eq!(
            duplicate_orchestrators_to_reap(&cands),
            vec!["wedged".to_string()]
        );
    }

    #[test]
    fn older_wins_when_scores_tie() {
        let cands = vec![c("old", 10, Some(1), None), c("new", 99, Some(1), None)];
        assert_eq!(pick_orchestrator_keeper(&cands).unwrap().id, "old");
        assert_eq!(
            duplicate_orchestrators_to_reap(&cands),
            vec!["new".to_string()]
        );
    }
}
