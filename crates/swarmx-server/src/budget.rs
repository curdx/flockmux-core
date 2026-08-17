//! Workspace budget brake (soft but real).
//!
//! The product's #1 trust gap: a swarm can silently burn subscription quota
//! and the user only finds out afterwards. This module enforces the optional
//! per-workspace cap (migration 0030): when a workspace's estimated all-time
//! cost reaches `budget_usd`, the brake TRIPS — every live agent of the
//! workspace is paused (same Ctrl-C + pause flag as the operator's interrupt),
//! the trip is persisted (marker + exactly which agents the brake paused), and
//! a `BudgetChanged` swarm event tells the UI. While the marker is set, new
//! spawns and new turn deliveries for the workspace fail closed with an
//! honest, actionable message. Raising the cap above the current estimate (or
//! clearing it) LIFTS the brake: the marker clears and only the brake-paused
//! agents resume — operator-paused agents were never recorded, so they stay
//! paused.
//!
//! Honesty red line: every amount here is an ESTIMATE scraped from session
//! transcripts and priced by the editable table in `routes/usage.rs` — the
//! same number `/api/usage` shows. It is NOT the subscription invoice. All
//! user-facing copy in this module says so (估算 / 不等于订阅账单).
//!
//! Fail-open on storage errors: the gates (`exceeded_error_*`) log and ALLOW
//! when the budget lookup itself fails — a storage hiccup must not brick all
//! spawns. The EXCEEDED state, once read, is always fail-closed. A trip whose
//! marker write FAILS is rolled back (the agents it paused are unpaused): a
//! half-trip — agents paused, no marker — would brick the workspace
//! invisibly, since paused agents produce no usage that could re-trigger the
//! check.

use crate::registry::Registry;
use swarmx_storage::Store;
use swarmx_swarm::{Swarm, SwarmEvent};

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The honest, actionable message every fail-closed gate returns. Chinese to
/// match the rest of the operator-facing error copy; carries the 估算 honesty
/// label and points at the page where the user lifts the brake.
fn exceeded_message(budget_usd: Option<f64>) -> String {
    let cap = budget_usd
        .map(|b| format!("${b:.2}"))
        .unwrap_or_else(|| "未设置".to_string());
    format!(
        "该工作空间已达预算上限（上限 {cap}；金额为估算，不等于订阅账单），成员已暂停。\
         到「用量」页调高预算或清除上限后，成员会自动恢复。"
    )
}

/// Spawn gate: Some(message) when the workspace's brake is ON and the caller
/// must fail closed. None = clear to proceed (no cap, not tripped, unknown
/// workspace — the spawn path's own 404 handles that — or lookup failure).
pub(crate) async fn exceeded_error_for_workspace(
    store: &Store,
    workspace_id: &str,
) -> Option<String> {
    match store
        .budget_gate_for_workspace(workspace_id.to_string())
        .await
    {
        Ok(Some(gate)) if gate.exceeded() => Some(exceeded_message(gate.budget_usd)),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(?e, workspace = %workspace_id, "budget gate lookup failed; allowing (fail-open)");
            None
        }
    }
}

/// Turn-delivery gate: same check keyed by the recipient agent.
pub(crate) async fn exceeded_error_for_agent(store: &Store, agent_id: &str) -> Option<String> {
    match store.budget_gate_for_agent(agent_id.to_string()).await {
        Ok(Some(gate)) if gate.exceeded() => Some(exceeded_message(gate.budget_usd)),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(?e, agent = %agent_id, "budget gate lookup failed; allowing (fail-open)");
            None
        }
    }
}

/// Trip the brake for a workspace: pause every live agent that isn't already
/// paused (operator-paused agents are NOT recorded — a later lift must leave
/// them paused), persist the marker + pause ledger, and broadcast
/// `BudgetChanged`. Idempotent: a concurrent/repeat trip keeps the first
/// marker and only unions pause rows; the event fires once (first trip).
/// When the trip must not stand — the cap moved mid-check or the marker
/// write failed — the pauses this attempt made are reverted (see
/// [`handle_trip_persist`]).
pub(crate) async fn trip_workspace(
    swarm: &Swarm,
    registry: &Registry,
    store: &Store,
    workspace_id: &str,
    budget_usd: Option<f64>,
    cost_usd: f64,
) {
    let now = now_ms();
    // Live agents of this workspace — same roster source as interrupt_all
    // (SQLite rows cross-referenced with the in-memory registry).
    let rows = match store.list_agents().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?e, workspace = %workspace_id, "budget trip: list_agents failed; aborting trip");
            return;
        }
    };
    let target_ids: Vec<String> = rows
        .into_iter()
        .filter(|row| row.killed_at.is_none())
        .filter(|row| row.workspace_id.as_deref() == Some(workspace_id))
        .map(|row| row.id)
        .collect();

    let mut paused_now: Vec<String> = Vec::new();
    for id in target_ids {
        // Only pause agents that are not ALREADY paused: an operator pause is
        // the user's own state and must survive the later lift. Benign race:
        // a user pause landing between this check and the Ctrl-C gets resumed
        // on lift — a one-instruction window, accepted.
        let already_paused = registry
            .get(&id)
            .map(|slot| {
                slot.lock()
                    .paused
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap_or(true); // not in the registry = not live = nothing to pause
        if already_paused {
            continue;
        }
        match crate::routes::rest::interrupt_one_inner(registry, swarm, &id).await {
            Ok(()) => paused_now.push(id),
            Err(msg) => {
                // Exited between the roster read and now — skip, don't abort.
                tracing::debug!(agent = %id, %msg, "budget trip: agent vanished mid-trip");
            }
        }
    }

    let outcome = store
        .trip_workspace_budget(workspace_id.to_string(), cost_usd, now, paused_now.clone())
        .await;
    handle_trip_persist(
        swarm,
        registry,
        workspace_id,
        budget_usd,
        cost_usd,
        now,
        &paused_now,
        outcome,
    )
    .await;
}

/// Undo the pauses one trip attempt made: flip the pause flag back off for
/// exactly the agents THIS attempt paused (operator-paused agents were never
/// touched, so they are never in `paused_now`). The flag flip alone
/// re-enables their auto-wake — they were running moments ago.
fn revert_pauses(registry: &Registry, paused_now: &[String]) {
    for id in paused_now {
        if let Some(slot) = registry.get(id) {
            slot.lock()
                .paused
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// React to the persist outcome of a trip attempt. Split out of
/// [`trip_workspace`] so the failure branches are testable without a broken
/// database (the store exposes no fault-injection hook, and every write goes
/// through the same pooled connection — a test can't make
/// `trip_workspace_budget` fail on demand through the public API).
async fn handle_trip_persist(
    swarm: &Swarm,
    registry: &Registry,
    workspace_id: &str,
    budget_usd: Option<f64>,
    cost_usd: f64,
    now: i64,
    paused_now: &[String],
    outcome: anyhow::Result<swarmx_storage::TripPersist>,
) {
    match outcome {
        Ok(swarmx_storage::TripPersist::Tripped) => {
            tracing::warn!(
                workspace = %workspace_id,
                cost_usd,
                budget_usd,
                paused = paused_now.len(),
                "BUDGET BRAKE TRIPPED (estimate) — workspace paused"
            );
            swarm.publish_event(SwarmEvent::BudgetChanged {
                workspace_id: workspace_id.to_string(),
                exceeded: true,
                budget_usd,
                cost_usd,
                at: now,
            });
        }
        Ok(swarmx_storage::TripPersist::AlreadyTripped) => {
            // A concurrent trip owns the marker (and already published the
            // event); our pause rows unioned into its ledger. Nothing to do.
        }
        Ok(swarmx_storage::TripPersist::BudgetMoved) => {
            // The user raised/cleared the cap between our cost check and the
            // marker write — the trip is stale. Nothing was persisted, so
            // revert the pauses we just made.
            tracing::info!(
                workspace = %workspace_id,
                "budget trip aborted: cap moved mid-check; unpausing the agents we just paused"
            );
            revert_pauses(registry, paused_now);
        }
        Err(e) => {
            // The marker didn't persist, so the trip must not stand: with no
            // marker the gates stay open and the banner stays dark, yet the
            // agents would sit paused forever — paused agents produce no new
            // usage, so `maybe_trip_after_usage` would never fire to retry
            // the trip. Revert the pauses THIS attempt made (operator pauses
            // were never touched), returning the workspace to its pre-trip
            // state; the next real usage event then genuinely re-runs the
            // check and retries the trip.
            tracing::warn!(
                ?e,
                workspace = %workspace_id,
                "budget trip: marker persist failed; reverting the pauses this attempt made"
            );
            revert_pauses(registry, paused_now);
        }
    }
}

/// Lift the brake: clear the marker, resume exactly the agents the brake had
/// paused (one manual wake each, same as the operator resume path), and
/// broadcast `BudgetChanged`. Callers invoke this when the cap is raised
/// above the current estimate or cleared.
pub(crate) async fn lift_workspace(
    swarm: &Swarm,
    registry: &Registry,
    store: &Store,
    server_url: &str,
    workspace_id: &str,
    budget_usd: Option<f64>,
    cost_usd: f64,
) {
    let paused = match store.lift_workspace_budget(workspace_id.to_string()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?e, workspace = %workspace_id, "budget lift: store failed; agents stay paused");
            return;
        }
    };
    for id in &paused {
        let Some(slot) = registry.get(id) else {
            continue; // exited while the brake was on — nothing to resume
        };
        slot.lock()
            .paused
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) =
            crate::wake::deliver_manual_wake(swarm, registry, server_url, id).await
        {
            tracing::debug!(?e, agent = %id, "budget lift: resume wake failed (agent likely exited)");
        }
    }
    if !paused.is_empty() {
        tracing::info!(
            workspace = %workspace_id,
            resumed = paused.len(),
            "budget brake lifted — brake-paused agents resumed"
        );
    }
    swarm.publish_event(SwarmEvent::BudgetChanged {
        workspace_id: workspace_id.to_string(),
        exceeded: false,
        budget_usd,
        cost_usd,
        at: now_ms(),
    });
}

/// Transcript-tailer hook: called after a batch of usage events lands for
/// `agent_id`. Cheap early-outs (no workspace / no cap / already tripped)
/// keep the per-turn cost to one indexed SELECT; the full cost estimate only
/// runs while a live cap exists and the brake hasn't tripped yet.
pub(crate) async fn maybe_trip_after_usage(
    swarm: &Swarm,
    registry: &Registry,
    store: &Store,
    agent_id: &str,
) {
    let gate = match store.budget_gate_for_agent(agent_id.to_string()).await {
        Ok(Some(g)) => g,
        Ok(None) => return,
        Err(e) => {
            tracing::debug!(?e, agent = %agent_id, "budget gate lookup failed; skipping trip check");
            return;
        }
    };
    if !gate.has_cap() || gate.exceeded() {
        return;
    }
    let budget = gate.budget_usd.unwrap_or(0.0);
    let (cost, _all_priced) =
        match crate::routes::usage::workspace_cost_estimate(store, &gate.workspace_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(?e, workspace = %gate.workspace_id, "budget cost estimate failed; skipping trip check");
                return;
            }
        };
    if cost >= budget {
        trip_workspace(
            swarm,
            registry,
            store,
            &gate.workspace_id,
            gate.budget_usd,
            cost,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AgentChannel, AgentSlot, Lifecycle};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use swarmx_pty::{PtyBridge, PtyHandles, SpawnOpts};

    async fn fresh_store() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(
            Store::open(&dir.path().join("swarmx.db"))
                .await
                .expect("open store"),
        );
        (dir, store)
    }

    async fn ws_with_agents(store: &Store, budget: Option<f64>, agent_ids: &[&str]) -> String {
        let ws = store
            .create_workspace(
                swarmx_storage::NewWorkspace {
                    name: "proj".into(),
                    cwd: "/tmp/proj".into(),
                    accent: None,
                },
                1,
            )
            .await
            .unwrap();
        store
            .set_workspace_budget(ws.id.clone(), budget)
            .await
            .unwrap();
        for (i, id) in agent_ids.iter().enumerate() {
            store
                .record_agent_spawn(swarmx_storage::NewAgent {
                    id: id.to_string(),
                    cli: "claude".into(),
                    role: "worker".into(),
                    workspace: "/tmp/proj".into(),
                    spawned_at: 10 + i as i64,
                    workspace_id: Some(ws.id.clone()),
                    spell_run_id: None,
                    thread_id: None,
                })
                .await
                .unwrap();
        }
        ws.id
    }

    /// Live-PTY slot on a child that survives Ctrl-C bytes long enough for
    /// the assertions (`sleep 30`); the test kills the bridge on teardown.
    /// Keep `output_rx` alive until after the kill — dropping it early wedges
    /// the child mid-exit (see wake.rs's live_pty_slot comment).
    #[cfg(unix)]
    fn live_slot(paused: bool) -> (AgentSlot, tokio::sync::mpsc::Receiver<bytes::Bytes>) {
        let PtyHandles { bridge, output_rx } = PtyBridge::spawn(SpawnOpts {
            argv: &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn test child");
        let input_tx = bridge.input_sender();
        let (lifecycle_tx, _rx) = tokio::sync::broadcast::channel(16);
        (
            AgentSlot {
                channel: AgentChannel::Pty {
                    bridge: Arc::new(bridge),
                    stream: Arc::new(crate::pty_stream::PtyStream::new()),
                    input_tx,
                },
                lifecycle: Arc::new(Mutex::new(Lifecycle::default())),
                lifecycle_tx,
                cli: "test".into(),
                role: "test".into(),
                workspace: "/tmp".into(),
                paused: Arc::new(AtomicBool::new(paused)),
                mcp_ready: tokio::sync::watch::channel(false).0,
                tui_http_port: None,
                serve_http_port: None,
                zulu: None,
                live_delivery: crate::input_delivery::LiveDelivery::Keystroke,
            },
            output_rx,
        )
    }

    fn paused_flag(registry: &Registry, id: &str) -> bool {
        registry
            .get(id)
            .unwrap()
            .lock()
            .paused
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn trip_pauses_live_unpaused_agents_marks_and_notifies() {
        let (dir, store) = fresh_store().await;
        let ws_id = ws_with_agents(&store, Some(10.0), &["a-1", "a-2", "a-3"]).await;
        let bb = dir.path().join("bb");
        std::fs::create_dir_all(&bb).unwrap();
        let swarm = Swarm::new(store.clone(), bb);
        let registry = Registry::new();
        let mut rxs = Vec::new();
        // a-2 is operator-paused BEFORE the trip — the brake must not claim it.
        for (id, paused) in [("a-1", false), ("a-2", true), ("a-3", false)] {
            let (slot, rx) = live_slot(paused);
            registry.insert(id.to_string(), slot);
            rxs.push(rx);
        }
        let mut events = swarm.subscribe();

        trip_workspace(&swarm, &registry, &store, &ws_id, Some(10.0), 10.25).await;

        assert!(paused_flag(&registry, "a-1"), "brake pauses live agents");
        assert!(paused_flag(&registry, "a-3"));
        // Marker persisted with the trip-time estimate.
        let gate = store
            .budget_gate_for_workspace(ws_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(gate.exceeded());
        let rec = store.get_workspace_by_id(ws_id.clone()).await.unwrap().unwrap();
        assert_eq!(rec.budget_exceeded_cost_usd, Some(10.25));
        // Pause ledger = ONLY the agents the brake paused (not operator-paused a-2).
        let ledger = store.lift_workspace_budget(ws_id.clone()).await.unwrap();
        assert_eq!(ledger, vec!["a-1".to_string(), "a-3".to_string()]);
        // The WS event fired on the first trip.
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(ev @ SwarmEvent::BudgetChanged { .. }) = events.recv().await {
                    break ev;
                }
            }
        })
        .await
        .expect("BudgetChanged event");
        match ev {
            SwarmEvent::BudgetChanged {
                workspace_id,
                exceeded,
                budget_usd,
                cost_usd,
                ..
            } => {
                assert_eq!(workspace_id, ws_id);
                assert!(exceeded);
                assert_eq!(budget_usd, Some(10.0));
                assert_eq!(cost_usd, 10.25);
            }
            _ => unreachable!(),
        }
        drop(registry);
        for rx in rxs {
            drop(rx);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn lift_resumes_only_brake_paused_agents() {
        let (dir, store) = fresh_store().await;
        let ws_id = ws_with_agents(&store, Some(10.0), &["a-1", "a-2"]).await;
        let bb = dir.path().join("bb");
        std::fs::create_dir_all(&bb).unwrap();
        let swarm = Swarm::new(store.clone(), bb);
        let registry = Registry::new();
        let mut rxs = Vec::new();
        for (id, paused) in [("a-1", false), ("a-2", true)] {
            let (slot, rx) = live_slot(paused);
            registry.insert(id.to_string(), slot);
            rxs.push(rx);
        }

        trip_workspace(&swarm, &registry, &store, &ws_id, Some(10.0), 11.0).await;
        assert!(paused_flag(&registry, "a-1"));
        assert!(paused_flag(&registry, "a-2"));

        // Budget raised above the estimate → lift. a-1 (brake-paused) resumes;
        // a-2 (operator-paused) STAYS paused.
        lift_workspace(
            &swarm,
            &registry,
            &store,
            "http://127.0.0.1:1",
            &ws_id,
            Some(20.0),
            11.0,
        )
        .await;
        assert!(!paused_flag(&registry, "a-1"), "brake-paused agent resumes");
        assert!(paused_flag(&registry, "a-2"), "operator-paused agent stays paused");
        let gate = store
            .budget_gate_for_workspace(ws_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(!gate.exceeded());

        drop(registry);
        for rx in rxs {
            drop(rx);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn trip_aborts_and_reverts_when_cap_moved_mid_check() {
        // The stored cap (20) is ABOVE the cost estimate (11) the trip was
        // attempted with — i.e. the user raised the cap between the check and
        // the write. The trip must not persist, and the pause it made
        // mid-flight must be reverted (flag back off, no marker, no ledger).
        let (dir, store) = fresh_store().await;
        let ws_id = ws_with_agents(&store, Some(20.0), &["a-1"]).await;
        let bb = dir.path().join("bb");
        std::fs::create_dir_all(&bb).unwrap();
        let swarm = Swarm::new(store.clone(), bb);
        let registry = Registry::new();
        let (slot, rx) = live_slot(false);
        registry.insert("a-1".to_string(), slot);

        trip_workspace(&swarm, &registry, &store, &ws_id, Some(20.0), 11.0).await;

        assert!(
            !paused_flag(&registry, "a-1"),
            "stale trip must revert the pause it made"
        );
        let gate = store
            .budget_gate_for_workspace(ws_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(!gate.exceeded(), "stale trip must not set the marker");
        assert!(
            store.lift_workspace_budget(ws_id.clone()).await.unwrap().is_empty(),
            "stale trip must not record pause rows"
        );

        drop(registry);
        drop(rx);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn persist_failure_reverts_pauses_and_a_later_trip_retries_clean() {
        // Simulated marker-write failure: the trip had paused a-1 through the
        // real interrupt path when the store returned Err. The attempt must
        // roll its pause back — no marker, no ledger, agent running — so the
        // NEXT trip (the "next usage event" retry) starts from the pre-trip
        // state instead of finding everyone silently paused with the gates
        // still open.
        let (dir, store) = fresh_store().await;
        let ws_id = ws_with_agents(&store, Some(10.0), &["a-1"]).await;
        let bb = dir.path().join("bb");
        std::fs::create_dir_all(&bb).unwrap();
        let swarm = Swarm::new(store.clone(), bb);
        let registry = Registry::new();
        let (slot, rx) = live_slot(false);
        registry.insert("a-1".to_string(), slot);

        crate::routes::rest::interrupt_one_inner(&registry, &swarm, "a-1")
            .await
            .unwrap();
        assert!(paused_flag(&registry, "a-1"), "pre-trip pause is in effect");

        handle_trip_persist(
            &swarm,
            &registry,
            &ws_id,
            Some(10.0),
            11.0,
            now_ms(),
            &["a-1".to_string()],
            Err(anyhow::anyhow!("simulated marker-write failure")),
        )
        .await;

        assert!(
            !paused_flag(&registry, "a-1"),
            "a failed persist must roll back the pause this attempt made"
        );
        let gate = store
            .budget_gate_for_workspace(ws_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(!gate.exceeded(), "no marker without a persisted trip");
        assert!(
            store.lift_workspace_budget(ws_id.clone()).await.unwrap().is_empty(),
            "no pause ledger without a persisted trip"
        );

        // Back at the pre-trip state, a later trip pauses + persists for real.
        trip_workspace(&swarm, &registry, &store, &ws_id, Some(10.0), 11.0).await;
        assert!(paused_flag(&registry, "a-1"), "the retry trips for real");
        let gate = store
            .budget_gate_for_workspace(ws_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(gate.exceeded(), "the retry persists the marker");

        drop(registry);
        drop(rx);
    }

    #[tokio::test]
    async fn gates_fail_closed_only_when_tripped() {
        let (_dir, store) = fresh_store().await;
        let ws_id = ws_with_agents(&store, Some(10.0), &["a-1"]).await;

        // Untripped: both gates clear.
        assert!(exceeded_error_for_workspace(&store, &ws_id).await.is_none());
        assert!(exceeded_error_for_agent(&store, "a-1").await.is_none());

        store
            .trip_workspace_budget(ws_id.clone(), 10.5, 1, vec!["a-1".into()])
            .await
            .unwrap();
        let msg = exceeded_error_for_workspace(&store, &ws_id)
            .await
            .expect("tripped workspace blocks spawns");
        assert!(msg.contains("预算上限"), "{msg}");
        assert!(msg.contains("估算"), "{msg}");
        let msg = exceeded_error_for_agent(&store, "a-1")
            .await
            .expect("tripped workspace blocks turn delivery");
        assert!(msg.contains("预算上限"), "{msg}");

        // Unknown ids / orphan agents: ungated (their own 404 paths apply).
        assert!(exceeded_error_for_workspace(&store, "nope").await.is_none());
        assert!(exceeded_error_for_agent(&store, "ghost").await.is_none());

        // Lift reopens the gates.
        store.lift_workspace_budget(ws_id.clone()).await.unwrap();
        assert!(exceeded_error_for_workspace(&store, &ws_id).await.is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn usage_hook_trips_when_estimate_crosses_cap() {
        let (dir, store) = fresh_store().await;
        let ws_id = ws_with_agents(&store, Some(0.000001), &["a-1"]).await;
        let bb = dir.path().join("bb");
        std::fs::create_dir_all(&bb).unwrap();
        let swarm = Swarm::new(store.clone(), bb);
        let registry = Registry::new();
        let (slot, rx) = live_slot(false);
        registry.insert("a-1".to_string(), slot);

        // No usage yet → estimate 0 < cap → no trip.
        maybe_trip_after_usage(&swarm, &registry, &store, "a-1").await;
        assert!(!paused_flag(&registry, "a-1"));

        // A priced usage event (claude-opus-4-1 is in the embedded table)
        // pushes the estimate past the tiny cap → trip.
        store
            .insert_agent_usage("a-1".into(), Some("claude-opus-4-1".into()), 1000, 0, 0, 0, 1)
            .await
            .unwrap();
        maybe_trip_after_usage(&swarm, &registry, &store, "a-1").await;
        assert!(paused_flag(&registry, "a-1"), "crossing the cap trips the brake");
        let gate = store
            .budget_gate_for_workspace(ws_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(gate.exceeded());

        drop(registry);
        drop(rx);
    }
}
