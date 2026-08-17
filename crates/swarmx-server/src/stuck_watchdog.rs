//! S5 stuck watchdog — the "alive-but-stuck" backstop (gap S5: a worker's
//! process stays alive but produces zero activity while the orchestrator
//! waits on its handoff forever).
//!
//! This detector was deferred once out of false-positive fear, so every
//! choice here is conservative over clever (宁慢勿误):
//!
//! - **Silence is never judged on wall-clock alone.** An agent is suspect
//!   only when it is silent past [`SILENCE_THRESHOLD`] on EVERY liveness
//!   channel (persisted activity / mailbox sends / token usage / blackboard
//!   writes / the in-memory activity ring) AND carries a structural pending
//!   obligation: a registered handoff key it still hasn't written, while not
//!   legitimately parked on undelivered deps. An idle orchestrator waiting
//!   for the user, or a worker waiting on its inputs, is quiet BY DESIGN and
//!   is never touched. (The M6d TTL scanner was removed for nudging exactly
//!   such healthy-but-quiet agents into fabricating handoffs.)
//! - **Soft state, never a kill.** A suspect gets ONE automatic wake through
//!   the existing wake path (`deliver_wake_turn` via
//!   [`crate::wake::deliver_watchdog_wake`]). Only after
//!   [`MAX_WATCHDOG_WAKES`] consecutive watchdog wakes produce no activity
//!   does it get marked — via the existing honest `last_error` channel with
//!   kind [`STUCK_ERROR_KIND`], which the UI renders as 疑似卡住 (amber),
//!   never as dead. Deliberately no `AgentState::Error`: the WakeCoordinator
//!   treats Error as producer death and would write `<handoff>.error`,
//!   kicking off a replan for an agent that may simply be slow.
//! - **A nudge is never silent.** Each delivered wake also publishes a
//!   `kind="watchdog"` `AgentActivity` (same channel the stuck mark uses) so
//!   the member rail / activity feed show 系统正在唤醒它 during the nudge
//!   window — otherwise the user sees ~30 minutes of nothing (10min silence
//!   + 2 wake windows) before the 疑似卡住 mark appears.
//! - **Any real activity clears everything** — the consecutive-wake count
//!   resets, and a persisted mark is cleared on the next tick (the recovery
//!   sweep is restart-safe: it reads the persisted mark, not in-memory
//!   state). Paused agents are excluded (their silence is deliberate).
//!
//! Design basis: docs/w2-3-stuck-detection-design-2026-06-15.md
//! (Chandra-Toueg / Temporal heartbeat separation / SWIM soft-suspect).

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use swarmx_protocol::ws_swarm::{AgentState, SwarmEvent};
use swarmx_swarm::Swarm;
use tokio::time::MissedTickBehavior;

/// How often the watchdog scans. 60s: the silence threshold is 10× that, so
/// the tick only sets detection granularity, never the false-positive rate.
const SCAN_INTERVAL: Duration = Duration::from_secs(60);

/// How long a live agent may show zero activity on every channel before the
/// watchdog first suspects it. THE knob — 10 min is deliberately above any
/// legitimate long turn chunk (deep think, big build, API retry storm); the
/// frontend's own "quiet for long" hint uses 15-30 min for reference. A
/// suspect is only ever woken at this cadence, so a slow-but-alive agent
/// pays at most one queued reminder turn per window, never a mark: marking
/// needs the silence to persist through [`MAX_WATCHDOG_WAKES`] more windows.
const SILENCE_THRESHOLD: Duration = Duration::from_secs(10 * 60);

/// Consecutive watchdog-issued wakes an agent may ignore (no activity on any
/// channel in between) before being marked as suspected-stuck. "Consecutive"
/// is literal: any real activity resets the count to zero.
const MAX_WATCHDOG_WAKES: u32 = 2;

/// `last_error_kind` value for the stuck mark. Reuses the existing honest
/// `last_error` channel (persisted, read by `list_agents` → `AgentInfo`, the
/// member rail, and the NeedsYou derivation) instead of a new field; the
/// frontend special-cases this kind into amber 疑似卡住 copy and a
/// non-auto-nudged NeedsYou lane.
pub(crate) const STUCK_ERROR_KIND: &str = "stuck";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Per-agent watchdog memory. In-memory only: on restart the count resets
/// (a marked agent keeps its persisted mark — the recovery sweep watches
/// that channel directly), which is the safe direction (more wakes before
/// re-marking, never fewer).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WatchState {
    /// Watchdog wakes delivered since the last observed activity.
    wakes_issued: u32,
    /// When the most recent watchdog wake was delivered (unix-ms). Doubles as
    /// the recovery baseline: any liveness signal strictly newer than this is
    /// proof of life.
    last_wake_at: i64,
    /// The stuck mark has been persisted — stop waking, stop re-marking.
    marked: bool,
}

/// Everything [`decide`] needs to judge one agent. Assembled per tick from
/// the agents row, the registry slot, the liveness probe, and the wake
/// tables. Pure data so the decision is unit-testable without IO.
#[derive(Debug, Clone)]
pub(crate) struct WatchSnapshot {
    /// Registry slot exists and the process is alive.
    alive: bool,
    /// Operator paused it — silence is deliberate, never suspect.
    paused: bool,
    /// Shim reported ready. `false` = still booting, the first-response
    /// watchdog owns that window.
    shim_ready: bool,
    /// agents row is live: `killed_at` and `shim_exit_at` both NULL.
    db_live: bool,
    /// `last_error` present (any kind, including our own mark) — another
    /// lane already owns this agent's honesty story.
    has_error: bool,
    /// A handoff key is registered for this agent AND neither it nor its
    /// `.error`/`.failed` alias is on the blackboard — someone is waiting on
    /// a delivery this agent hasn't made (the S5 shape).
    owes_handoff: bool,
    /// At least one subscribed wake key (or its aliases) is absent from the
    /// blackboard — the agent is legitimately parked on inputs.
    waiting_on_deps: bool,
    /// Spawn time (unix-ms) — the silence anchor for a never-active agent.
    spawned_at: i64,
    /// Newest sign of life across every channel (persisted + in-memory
    /// ring). `None` = never produced anything.
    last_signal_at: Option<i64>,
}

/// What the watchdog should do with one agent this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchVerdict {
    /// Nothing: healthy, legitimately idle, or another lane's business.
    Quiet,
    /// Deliver one watchdog wake.
    Wake,
    /// Wakes exhausted — persist the stuck mark.
    Mark,
    /// Activity newer than our last wake — reset the consecutive count (the
    /// driver also drops any in-memory state; a persisted mark is cleared by
    /// the recovery sweep).
    Recovered,
}

/// The watchdog's entire decision, pure and unit-tested. Ordering matters:
/// recovery beats exclusion, exclusion beats the silence clock, the clock
/// beats the structural check, and only all-five-green escalates.
pub(crate) fn decide(
    snap: &WatchSnapshot,
    state: Option<WatchState>,
    now: i64,
    threshold_ms: i64,
    max_wakes: u32,
) -> WatchVerdict {
    // Recovery first: a signal strictly newer than our last wake proves the
    // agent reacted (or was never really wedged). Resets "consecutive".
    if let (Some(st), Some(sig)) = (state, snap.last_signal_at) {
        if sig > st.last_wake_at {
            return WatchVerdict::Recovered;
        }
    }
    // Hard exclusions — the reaper (dead), the operator (paused), the
    // first-response watchdog (never ready), or another error lane.
    if !snap.alive || snap.paused || !snap.shim_ready || !snap.db_live || snap.has_error {
        return WatchVerdict::Quiet;
    }
    // The silence clock runs from the newest of: last sign of life, our last
    // wake (a wake we just issued gets a full window to land before we count
    // the next one), and spawn (a fresh agent is still booting its first
    // turn — never silent by definition).
    let mut anchor = snap.spawned_at;
    if let Some(sig) = snap.last_signal_at {
        anchor = anchor.max(sig);
    }
    if let Some(st) = state {
        anchor = anchor.max(st.last_wake_at);
    }
    if now - anchor <= threshold_ms {
        return WatchVerdict::Quiet;
    }
    // Structural pending (S5): only an agent that OWES an unwritten handoff
    // and is NOT parked on undelivered deps may be suspected. This is what
    // keeps the detector off idle orchestrators and waiting workers — the
    // false-positive class that killed the M6d TTL scanner.
    if !snap.owes_handoff || snap.waiting_on_deps {
        return WatchVerdict::Quiet;
    }
    match state {
        // Mark once, then hands off (the recovery sweep owns it from here).
        Some(st) if st.marked => WatchVerdict::Quiet,
        Some(st) if st.wakes_issued >= max_wakes => WatchVerdict::Mark,
        _ => WatchVerdict::Wake,
    }
}

/// Spawn the watchdog for the whole process. Same lifecycle pattern as the
/// reaper: the task runs until shutdown; the caller drops the JoinHandle.
pub fn spawn(state: crate::AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SCAN_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut states: HashMap<String, WatchState> = HashMap::new();
        loop {
            tick.tick().await;
            sweep(&state, &mut states).await;
        }
    });
}

/// Newest sign of life for one agent: the persisted channels (already the
/// max across activity/mail/usage/blackboard) OR'd with the in-memory
/// activity ring (opencode/reasonix/zulu push tool activity to the ring via
/// the ingress POST without touching `last_activity_at`). 0 = never active.
fn latest_signal(
    swarm: &Swarm,
    persisted: &HashMap<String, i64>,
    agent_id: &str,
) -> i64 {
    let ring = swarm
        .recent_activity(agent_id)
        .iter()
        .map(|rec| rec.at)
        .max()
        .unwrap_or(0);
    persisted.get(agent_id).copied().unwrap_or(0).max(ring)
}

/// Is `key` — or its `.error` / `.failed` failure alias — on the blackboard?
/// Mirrors `WakeCoordinator::key_or_alias_written`.
async fn key_or_alias_present(swarm: &Swarm, key: &str) -> bool {
    for probe in [
        key.to_string(),
        format!("{key}.error"),
        format!("{key}.failed"),
    ] {
        if matches!(swarm.read_blackboard(&probe).await, Ok(Some(_))) {
            return true;
        }
    }
    false
}

/// One watchdog pass. Fail-safe throughout: any store error aborts the pass
/// (doing nothing is always the safe direction for this detector).
async fn sweep(app: &crate::AppState, states: &mut HashMap<String, WatchState>) {
    let now = now_ms();
    let rows = match app.store.list_agents().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?e, "stuck watchdog: list_agents failed; skipping tick");
            return;
        }
    };
    let row_live =
        |r: &swarmx_storage::AgentRecord| r.killed_at.is_none() && r.shim_exit_at.is_none();

    // ── Pass 1: recovery sweep for persisted stuck marks ─────────────────
    // Covers agents marked by THIS process (all engines — only claude/codex
    // have a transcript tailer that would also clear) and, restart-safely,
    // marks left by a PREVIOUS process. Any sign of life strictly newer than
    // the mark clears it: "疑似卡住" must never outlive real activity.
    let marked: Vec<&swarmx_storage::AgentRecord> = rows
        .iter()
        .filter(|r| row_live(r) && r.last_error_kind.as_deref() == Some(STUCK_ERROR_KIND))
        .collect();
    if !marked.is_empty() {
        let ids: Vec<String> = marked.iter().map(|r| r.id.clone()).collect();
        match app.store.latest_liveness_signals(ids).await {
            Ok(signals) => {
                for r in marked {
                    let latest = latest_signal(&app.swarm, &signals, &r.id);
                    if latest > r.last_error_at.unwrap_or(0) {
                        if let Err(e) = app.store.clear_agent_error(r.id.clone()).await {
                            tracing::warn!(?e, agent = %r.id, "stuck watchdog: clear_agent_error failed");
                            continue;
                        }
                        states.remove(&r.id);
                        app.swarm.publish_event(SwarmEvent::AgentState {
                            agent_id: r.id.clone(),
                            state: AgentState::Idle,
                        });
                        tracing::info!(
                            agent = %r.id,
                            "stuck watchdog: activity resumed; cleared the stuck mark"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(?e, "stuck watchdog: liveness probe failed; recovery sweep skipped")
            }
        }
    }

    // Forget agents that are gone (killed / exited / torn down) so their
    // consecutive counts can't leak into a future same-id incarnation.
    states.retain(|id, _| {
        rows.iter().any(|r| r.id == *id && row_live(r))
    });

    // ── Pass 2: candidacy (cheap gates first, blackboard IO last) ────────
    let mut candidate_ids: Vec<String> = Vec::new();
    for r in rows.iter().filter(|r| row_live(r)) {
        if r.shim_ready_at.is_none() {
            continue; // still booting — the first-response watchdog owns it
        }
        if r.last_error.is_some() {
            // Another error lane owns this agent's honesty story (auth /
            // rate_limit / first-response / our own mark). Reset any
            // consecutive count so a later recovery starts the sequence fresh.
            states.remove(&r.id);
            continue;
        }
        let Some(slot) = app.registry.get(&r.id) else {
            continue; // no live slot — the reaper / orphan ledger's business
        };
        let (alive, paused) = {
            let g = slot.lock();
            (
                g.is_alive(),
                g.paused.load(std::sync::atomic::Ordering::Relaxed),
            )
        };
        if !alive || paused {
            continue;
        }
        candidate_ids.push(r.id.clone());
    }
    if candidate_ids.is_empty() {
        return;
    }
    // A failed liveness probe must NOT read as "everyone silent" — that
    // inversion is how watchdogs cry wolf. Skip the tick instead.
    let signals = match app.store.latest_liveness_signals(candidate_ids.clone()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?e, "stuck watchdog: liveness probe failed; skipping tick");
            return;
        }
    };
    let exit_keys = app.exit_keys.read().await.clone();
    let subs = app.wake_subs.read().await.clone();
    let threshold_ms = SILENCE_THRESHOLD.as_millis() as i64;

    for id in candidate_ids {
        let row = rows.iter().find(|r| r.id == id).expect("candidate row");
        let tracked = states.get(&id).copied();
        let raw_signal = latest_signal(&app.swarm, &signals, &id);
        let last_signal = (raw_signal > 0).then_some(raw_signal);

        // Cheap exits, mirroring decide()'s ordering, so the blackboard
        // reads below run only for agents silent past the threshold.
        if let (Some(st), Some(sig)) = (tracked, last_signal) {
            if sig > st.last_wake_at {
                states.remove(&id);
                tracing::info!(
                    agent = %id,
                    "stuck watchdog: activity after wake; consecutive count reset"
                );
                continue;
            }
        }
        let mut anchor = row.spawned_at;
        if let Some(sig) = last_signal {
            anchor = anchor.max(sig);
        }
        if let Some(st) = tracked {
            anchor = anchor.max(st.last_wake_at);
        }
        if now - anchor <= threshold_ms {
            continue;
        }

        // Structural pending (S5): the agent must OWE an unwritten handoff…
        let owed_key = match exit_keys.get(&id) {
            Some(ek) if !key_or_alias_present(&app.swarm, &ek.handoff_signal).await => {
                Some(ek.handoff_signal.clone())
            }
            _ => None,
        };
        // …and must not be parked on undelivered deps (a waiting worker is
        // quiet BY DESIGN — waking it is the M6d false-positive class).
        let mut waiting_on_deps = false;
        if let Some(keys) = subs.get(&id) {
            for k in keys {
                if !key_or_alias_present(&app.swarm, k).await {
                    waiting_on_deps = true;
                    break;
                }
            }
        }

        let snap = WatchSnapshot {
            alive: true,
            paused: false,
            shim_ready: true,
            db_live: true,
            has_error: false,
            owes_handoff: owed_key.is_some(),
            waiting_on_deps,
            spawned_at: row.spawned_at,
            last_signal_at: last_signal,
        };
        match decide(&snap, tracked, now, threshold_ms, MAX_WATCHDOG_WAKES) {
            WatchVerdict::Wake => {
                let key = owed_key.clone().unwrap_or_default();
                let wakes = tracked.map(|s| s.wakes_issued).unwrap_or(0) + 1;
                tracing::warn!(
                    agent = %id,
                    handoff = %key,
                    wake = wakes,
                    "stuck watchdog: silent agent owes a handoff; delivering watchdog wake"
                );
                match crate::wake::deliver_watchdog_wake(
                    &app.swarm,
                    &app.registry,
                    &app.server_url,
                    &id,
                    &key,
                    SILENCE_THRESHOLD.as_secs() / 60,
                )
                .await
                {
                    Ok(()) => {
                        // The nudge must be VISIBLE: same AgentActivity
                        // channel the stuck mark uses, so the member rail /
                        // activity feed show 系统正在唤醒它 during the nudge
                        // window instead of ~30min of silence until the mark.
                        publish_nudge_event(&app.swarm, &id, wakes, &key, now);
                    }
                    Err(e) => {
                        // Counted anyway — the cap bounds total pokes per silence
                        // streak even when delivery itself is what's broken.
                        tracing::warn!(
                            ?e,
                            agent = %id,
                            "stuck watchdog: wake delivery failed (still counts toward the wake cap)"
                        );
                    }
                }
                states.insert(
                    id.clone(),
                    WatchState {
                        wakes_issued: wakes,
                        last_wake_at: now,
                        marked: false,
                    },
                );
            }
            WatchVerdict::Mark => {
                let key = owed_key.clone().unwrap_or_default();
                let reason = format!(
                    "疑似卡住：进程还活着，但连续 {} 次系统唤醒后仍无任何活动迹象，交付键 `{key}` 未写。\
                     可能卡在授权弹窗 / 网络 / 等待输入——系统不会自动终止它；\
                     请打开它的终端查看，可手动唤醒或终止。",
                    MAX_WATCHDOG_WAKES
                );
                if let Err(e) = app
                    .store
                    .record_agent_error(id.clone(), reason.clone(), STUCK_ERROR_KIND, now)
                    .await
                {
                    tracing::warn!(
                        ?e,
                        agent = %id,
                        "stuck watchdog: record_agent_error failed; will retry next tick"
                    );
                    continue;
                }
                // Honest resting-state correction (interrupt_one_inner
                // precedent): Idle is the truthful claim after this much
                // silence, and the state change is what bumps the frontend
                // roster refetch so the persisted mark surfaces. Deliberately
                // NOT AgentState::Error — see the module doc.
                app.swarm.publish_event(SwarmEvent::AgentState {
                    agent_id: id.clone(),
                    state: AgentState::Idle,
                });
                app.swarm.publish_event(SwarmEvent::AgentActivity {
                    agent_id: id.clone(),
                    kind: "system".to_string(),
                    label: reason,
                    phase: "error".to_string(),
                    seq: 0,
                    duration_ms: None,
                    at: now,
                });
                let prev = tracked.unwrap_or_default();
                states.insert(
                    id.clone(),
                    WatchState {
                        wakes_issued: prev.wakes_issued.max(MAX_WATCHDOG_WAKES),
                        last_wake_at: prev.last_wake_at,
                        marked: true,
                    },
                );
                tracing::warn!(
                    agent = %id,
                    handoff = %key,
                    "stuck watchdog: marked agent as suspected-stuck after ignored wakes"
                );
            }
            // Unreachable (mirrored above) but kept total: decide() is the
            // single tested source of truth for the recovery rule.
            WatchVerdict::Recovered => {
                states.remove(&id);
            }
            WatchVerdict::Quiet => {}
        }
    }
}

/// The UI-visible half of a delivered watchdog wake: an `AgentActivity` on
/// the same channel the stuck mark uses, but kind `watchdog` (not `system`)
/// so the frontend can tell a nudge apart from a tool step or an error mark,
/// and phase `ok` — the wake WAS delivered; `running` would paint a spinner
/// over an agent that is NOT working, `error` would cry wolf. `seq: 0`
/// follows the other server-side system events (the transcript tailer's tool
/// events start at 1), so a newer system event replaces the older one in the
/// UI's per-agent log instead of filling it with pokes.
fn publish_nudge_event(swarm: &Swarm, agent_id: &str, wakes: u32, handoff_key: &str, at: i64) {
    swarm.publish_event(SwarmEvent::AgentActivity {
        agent_id: agent_id.to_string(),
        kind: "watchdog".to_string(),
        label: format!(
            "系统正在唤醒它（第 {wakes}/{MAX_WATCHDOG_WAKES} 次）：已超过 {} 分钟无活动，\
             交付键 `{handoff_key}` 仍未写；连续 {MAX_WATCHDOG_WAKES} 次唤醒仍无响应将标记「疑似卡住」。",
            SILENCE_THRESHOLD.as_secs() / 60,
        ),
        phase: "ok".to_string(),
        seq: 0,
        duration_ms: None,
        at,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: i64 = 600_000; // 10 min
    const NOW: i64 = 1_800_000_000_000;

    fn snap() -> WatchSnapshot {
        // The S5 shape: alive, ready, no error, owes a handoff, not waiting.
        WatchSnapshot {
            alive: true,
            paused: false,
            shim_ready: true,
            db_live: true,
            has_error: false,
            owes_handoff: true,
            waiting_on_deps: false,
            spawned_at: NOW - 3_600_000,
            last_signal_at: Some(NOW - 1_800_000),
        }
    }

    fn st(wakes: u32, last_wake_at: i64) -> Option<WatchState> {
        Some(WatchState {
            wakes_issued: wakes,
            last_wake_at,
            marked: false,
        })
    }

    #[test]
    fn silent_worker_owing_handoff_gets_one_wake() {
        assert_eq!(
            decide(&snap(), None, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Wake
        );
    }

    #[test]
    fn second_consecutive_silence_window_gets_second_wake() {
        // Wake #1 delivered at T; a full threshold later with zero activity.
        let state = st(1, NOW - THRESHOLD - 1);
        assert_eq!(
            decide(&snap(), state, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Wake
        );
    }

    #[test]
    fn two_ignored_wakes_mark_stalled() {
        let state = st(2, NOW - THRESHOLD - 1);
        assert_eq!(
            decide(&snap(), state, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Mark
        );
    }

    #[test]
    fn marked_agents_are_never_re_marked_or_re_woken() {
        // No signal newer than the last wake (else it would be Recovered).
        let state = Some(WatchState {
            wakes_issued: 2,
            last_wake_at: NOW - 2 * THRESHOLD,
            marked: true,
        });
        assert_eq!(
            decide(&snap(), state, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Quiet
        );
    }

    #[test]
    fn any_activity_after_a_wake_resets_the_sequence() {
        let state = st(1, NOW - 120_000);
        let mut s = snap();
        s.last_signal_at = Some(NOW - 60_000); // newer than the last wake
        assert_eq!(
            decide(&s, state, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Recovered
        );
    }

    #[test]
    fn silence_within_threshold_is_quiet() {
        let mut s = snap();
        s.last_signal_at = Some(NOW - THRESHOLD + 1);
        assert_eq!(
            decide(&s, None, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Quiet
        );
    }

    #[test]
    fn a_just_issued_wake_gets_a_full_window_before_the_next() {
        // Wake #1 went out 60s ago (a scan tick); no activity yet — the
        // second wake must wait for the FULL threshold since that wake.
        let state = st(1, NOW - 60_000);
        let mut s = snap();
        s.last_signal_at = Some(NOW - 1_800_000);
        assert_eq!(
            decide(&s, state, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Quiet
        );
    }

    #[test]
    fn fresh_spawn_is_never_silent() {
        let mut s = snap();
        s.spawned_at = NOW - 60_000;
        s.last_signal_at = None;
        assert_eq!(
            decide(&s, None, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Quiet
        );
    }

    #[test]
    fn idle_agent_without_handoff_obligation_is_never_touched() {
        // The orchestrator waiting on the user: alive, silent for hours —
        // quiet BY DESIGN. owes_handoff=false is the whole S5 gate.
        let mut s = snap();
        s.owes_handoff = false;
        s.last_signal_at = Some(NOW - 86_400_000);
        assert_eq!(
            decide(&s, None, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Quiet
        );
    }

    #[test]
    fn worker_parked_on_undelivered_deps_is_never_touched() {
        let mut s = snap();
        s.waiting_on_deps = true;
        assert_eq!(
            decide(&s, None, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Quiet
        );
    }

    #[test]
    fn paused_dead_unready_and_errored_are_all_excluded() {
        for mutate in [
            (|s: &mut WatchSnapshot| s.paused = true) as fn(&mut WatchSnapshot),
            |s| s.alive = false,
            |s| s.shim_ready = false,
            |s| s.db_live = false,
            |s| s.has_error = true,
        ] {
            let mut s = snap();
            mutate(&mut s);
            assert_eq!(
                decide(&s, None, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
                WatchVerdict::Quiet
            );
        }
    }

    #[test]
    fn recovery_beats_exclusion() {
        // Marked AND has_error (the persisted mark) — a fresh signal still
        // reads as Recovered first so the state resets.
        let mut s = snap();
        s.has_error = true;
        s.last_signal_at = Some(NOW - 1_000);
        let state = Some(WatchState {
            wakes_issued: 2,
            last_wake_at: NOW - 120_000,
            marked: true,
        });
        assert_eq!(
            decide(&s, state, NOW, THRESHOLD, MAX_WATCHDOG_WAKES),
            WatchVerdict::Recovered
        );
    }

    #[tokio::test]
    async fn delivered_wake_publishes_a_visible_nudge_event() {
        // The Wake branch's UI half: a delivered watchdog wake must surface
        // on the same AgentActivity channel the stuck mark uses, distinguished
        // by kind="watchdog", so the member rail / activity feed show
        // 系统正在唤醒它 during the nudge window.
        let dir = tempfile::TempDir::new().unwrap();
        let store = std::sync::Arc::new(
            swarmx_storage::Store::open(&dir.path().join("swarmx.db"))
                .await
                .expect("open store"),
        );
        let bb = dir.path().join("bb");
        std::fs::create_dir_all(&bb).unwrap();
        let swarm = Swarm::new(store, bb);
        let mut events = swarm.subscribe();

        publish_nudge_event(&swarm, "a-1", 1, "ws/thread/worker.done", NOW);

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("nudge event arrives")
            .expect("event stream open");
        match ev {
            SwarmEvent::AgentActivity {
                agent_id,
                kind,
                label,
                phase,
                ..
            } => {
                assert_eq!(agent_id, "a-1");
                assert_eq!(kind, "watchdog");
                assert_eq!(phase, "ok");
                assert!(label.contains("系统正在唤醒它"), "{label}");
                assert!(label.contains("第 1/2 次"), "{label}");
                assert!(label.contains("ws/thread/worker.done"), "{label}");
            }
            other => panic!("expected AgentActivity, got {other:?}"),
        }
    }
}
