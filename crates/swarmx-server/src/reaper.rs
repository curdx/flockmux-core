//! Liveness reaper — defense-in-depth against agents stuck forever "alive".
//!
//! Two paths already retire an agent: `scan_osc` emits a `ShimExit` when the
//! shim prints its OSC exit marker, and the PTY pump synthesizes one on EOF
//! (spawn.rs). But a shim killed by SIGKILL / OOM — or a pump task that died
//! before it could 补发 — can leave the registry believing a child is alive,
//! showing a green dot + "正在响应" indefinitely. That is exactly the fake
//! state the honesty work is removing.
//!
//! This periodic sweep is the backstop. Every few seconds it asks each live
//! `PtyBridge` `is_alive()` — a deterministic `waitpid`, never a false positive
//! — and for any child that has actually exited without a recorded `ShimExit`,
//! it synthesizes one: latching `lifecycle.shim_exit`, relaying to live PTY
//! subscribers, persisting via `record_shim_exit`, and publishing the
//! `AgentState` change — the same effects the WS lifecycle consumer produces,
//! but without depending on a WS client being attached.
//!
//! The sweep is also the process-LEDGER writer (migration 0029): the first
//! time it sees each slot it persists the shim's pid/pgid into the agents
//! row. That closes the structural hole where a server crash/SIGKILL left the
//! real shim → CLI tree reparented to init, still burning subscription quota,
//! while `mark_orphan_agents_killed` only flipped the DB row. The reaper is
//! the convergence point that already has both halves of the data flow (the
//! slot → `bridge.pid()`, and the store), so no route-layer change is needed.
//! The boot half of the ledger is [`reap_orphan_processes`]: at startup it
//! really kills the process trees behind the rows the orphan sweep settled.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use swarmx_protocol::ws_swarm::{AgentState, SwarmEvent};
use swarmx_storage::{OrphanedAgentProcess, Store};
use swarmx_swarm::Swarm;
use parking_lot::Mutex;
use tokio::time::MissedTickBehavior;

use crate::registry::{AgentSlot, LifecycleEvent, Registry};

/// How often the reaper sweeps the registry. 5s keeps the worst-case lag
/// between "process actually died" and "UI stops lying" well under the 10s the
/// honesty work targets, without meaningfully waking the runtime.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Grace between a slot's exit being recorded and its eviction from the
/// registry. Long enough that a just-detached UI can reattach for the final
/// bytes; short enough that dead slots don't accumulate. Eviction reclaims the
/// three resources a self-exited agent otherwise leaks forever: the parked PTY
/// writer thread, its master fd, and a slot against the live-agent cap (which is
/// literally `registry.list().len()`). The persisted SQLite row stays the source
/// of truth for the exited-agent list the UI renders — the registry holds only
/// live-or-recently-exited agents.
const REAP_GRACE: Duration = Duration::from_secs(60);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Spawn the liveness reaper for the whole process. The task exits only at
/// shutdown; callers drop the handle (same pattern as the wake coordinator).
///
/// Also enforces the captain singleton: if a direction somehow got two live
/// orchestrators (race before the spawn gate, old builds), tear the extras
/// down automatically — users never have to notice or click.
pub fn spawn(state: crate::AppState) {
    tokio::spawn(async move {
        // Startup, before the first sweep: really kill the process trees the
        // PREVIOUS server left live when it died (their rows were just
        // settled by main.rs's `mark_orphan_agents_killed` — a DB-only mark
        // that never signalled a single process).
        reap_orphan_processes(&state.store).await;
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Per-agent clock started when a slot's exit is first observed; drives
        // the REAP_GRACE eviction below.
        let mut exited_at: HashMap<String, Instant> = HashMap::new();
        // Agent ids whose shim pid/pgid is already persisted (migration 0029
        // ledger). Written once per agent, on the first sweep that sees the
        // slot — at most ~5s after spawn.
        let mut pids_recorded: HashSet<String> = HashSet::new();
        loop {
            tick.tick().await;
            sweep_once(
                &state.registry,
                &state.store,
                &state.swarm,
                &mut exited_at,
                &mut pids_recorded,
                REAP_GRACE,
            )
            .await;
            reap_extra_orchestrators(&state).await;
        }
    });
}

/// Tear down every live orchestrator that is NOT the keeper in its direction.
/// Sole captains (even soft-watchdog) are left alone — recovery via `/login`
/// still works. Duplicates are always a product bug; kill without asking.
async fn reap_extra_orchestrators(state: &crate::AppState) {
    let agents = match state.store.list_agents().await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(?e, "reaper: list_agents for orch singleton failed");
            return;
        }
    };
    let mut groups: HashMap<(String, String), Vec<crate::agent_lifecycle::OrchCandidate>> =
        HashMap::new();
    for a in agents {
        if a.role != "orchestrator" || a.killed_at.is_some() || a.shim_exit_at.is_some() {
            continue;
        }
        let Some(ws) = a.workspace_id.clone() else {
            continue;
        };
        let tid = a.thread_id.clone().unwrap_or_default();
        groups.entry((ws, tid)).or_default().push(
            crate::agent_lifecycle::OrchCandidate {
                id: a.id,
                spawned_at: a.spawned_at,
                last_activity_at: a.last_activity_at,
                last_error_kind: a.last_error_kind,
            },
        );
    }
    for ((ws, tid), cands) in groups {
        let victims = crate::agent_lifecycle::duplicate_orchestrators_to_reap(&cands);
        for id in victims {
            tracing::warn!(
                agent = %id,
                workspace = %ws,
                thread = %tid,
                "reaper: tearing down duplicate orchestrator (keep one captain)"
            );
            crate::routes::rest::teardown_agent(state, &id).await;
        }
    }
}

/// One pass over the registry. The exit-code persist/publish is done OUTSIDE
/// the slot lock (`detect_exit` releases it before returning) so we never hold
/// a `parking_lot` mutex across the `.await`.
async fn sweep_once(
    registry: &Registry,
    store: &Store,
    swarm: &Swarm,
    exited_at: &mut HashMap<String, Instant>,
    pids_recorded: &mut HashSet<String>,
    grace: Duration,
) {
    let mut present: HashSet<String> = HashSet::new();
    for (agent_id, slot_arc) in registry.list() {
        present.insert(agent_id.clone());

        // Process ledger: persist the shim's pid/pgid on first sight of each
        // slot, so a later server crash leaves enough on the row for the next
        // boot's `reap_orphan_processes` to really kill the orphaned tree.
        // First-write-wins in the store; a missing row (record_agent_spawn
        // still racing us) just retries next sweep.
        if !pids_recorded.contains(&agent_id) {
            let pid = slot_arc.lock().shim_pid();
            if let Some(pid) = pid {
                match store
                    .record_agent_shim_pid(agent_id.clone(), pid as i64, pgid_of(pid))
                    .await
                {
                    Ok(true) => {
                        pids_recorded.insert(agent_id.clone());
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::debug!(?e, agent = %agent_id, "reaper: record_agent_shim_pid failed")
                    }
                }
            }
        }

        if let Some(code) = detect_exit(slot_arc.as_ref()) {
            let at = now_ms();
            if let Err(e) = store.record_shim_exit(agent_id.clone(), code, at).await {
                tracing::warn!(?e, agent = %agent_id, "reaper: record_shim_exit failed");
            }
            // Non-zero exit = abnormal death → Error (red, sorted to top); clean
            // exit → Exited. Intentional kills also exit non-zero, but those rows
            // carry `killed_at`, which the UI prioritizes over this.
            let next = if code == 0 {
                AgentState::Exited
            } else {
                AgentState::Error
            };
            swarm.publish_event(SwarmEvent::AgentState {
                agent_id: agent_id.clone(),
                state: next,
            });
            tracing::info!(
                agent = %agent_id,
                code,
                "reaper: child exited without a ShimExit marker; synthesized one"
            );
            // Start the eviction clock now that the exit is recorded.
            exited_at.insert(agent_id.clone(), Instant::now());
            continue;
        }

        // Already-accounted exit (recorded elsewhere too — the WS lifecycle
        // consumer or the pump's EOF). Evict after the grace so the parked writer
        // thread, master fd, and live-agent-cap slot are reclaimed instead of
        // leaking until process exit. Dropping the registry's `Arc` at end of
        // this iteration is what actually reaps them.
        if is_reapable(slot_arc.as_ref()) {
            let since = *exited_at.entry(agent_id.clone()).or_insert_with(Instant::now);
            if since.elapsed() >= grace {
                registry.remove(&agent_id);
                exited_at.remove(&agent_id);
                tracing::info!(
                    agent = %agent_id,
                    "reaper: evicted exited slot (reclaimed writer thread + master fd + live-agent slot)"
                );
            }
        }
    }
    // Forget clocks for agents already gone from the registry (teardown / auto-kill).
    exited_at.retain(|id, _| present.contains(id));
    pids_recorded.retain(|id| present.contains(id));
}

/// Inspect one slot. If its child has exited but no `ShimExit` was ever
/// recorded, latch `lifecycle.shim_exit`, relay to live subscribers, and
/// return the exit code to persist. Returns `None` when the agent is still
/// alive or its exit was already accounted for (so a later sweep or a racing
/// WS consumer never double-counts). Synchronous — the slot lock is dropped
/// when this returns, before the caller's DB write.
fn detect_exit(slot: &Mutex<AgentSlot>) -> Option<i32> {
    let slot = slot.lock();
    if slot.lifecycle.lock().shim_exit.is_some() {
        return None;
    }
    if slot.is_alive() {
        return None;
    }
    let code = slot.try_exit_code().unwrap_or(-1);
    slot.lifecycle.lock().shim_exit = Some(code);
    // Relay to any live PTY subscriber so it updates immediately; its consumer
    // is idempotent with the direct persist/publish the caller does.
    let _ = slot.lifecycle_tx.send(LifecycleEvent::ShimExit(code));
    Some(code)
}

/// True when a slot's exit is already recorded (`shim_exit` latched) and its
/// child is confirmed dead — i.e. a spent slot safe to evict from the registry
/// after the grace. Distinct from `detect_exit`: that one *accounts* a new death
/// (and skips already-latched slots); this one *identifies* an already-accounted
/// death to reclaim, so the two compose across sweeps without double-counting.
fn is_reapable(slot: &Mutex<AgentSlot>) -> bool {
    let slot = slot.lock();
    slot.lifecycle.lock().shim_exit.is_some() && !slot.is_alive()
}

// ── startup orphan process reaping ──────────────────────────────────────────
//
// main.rs's `mark_orphan_agents_killed` settles the ROWS a dead server left
// "live"; this section settles the PROCESSES. A crash/SIGKILL orphans the
// shim → CLI → descendants tree to init, where it keeps burning subscription
// quota with nobody watching. For each row the boot sweep just settled we:
// probe the recorded shim pid → VERIFY the process is still one of ours
// (never kill on a bare pid match — pids get recycled) → signal the whole
// process group (unix) / tree (Windows).

/// How far behind now a row's `killed_at` may lie to count as "settled by
/// THIS boot's orphan sweep". main.rs stamps the orphans seconds before the
/// reaper task starts, so a small window comfortably contains exactly that
/// set — and the window is the safety property: it keeps agents intentionally
/// killed in an OLD session (whose pids may since have been recycled by an
/// unrelated process) out of the kill set. 宁漏杀不误杀.
const ORPHAN_REAP_WINDOW_MS: i64 = 120_000;

/// What [`reap_one_orphan`] concluded about one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanReap {
    /// Pid already gone (clean exits, previously-reaped rows) — nothing to do.
    NotRunning,
    /// The pid is alive but does not look like the agent we recorded
    /// (cmdline / pgid / image-name mismatch): the pid was recycled by an
    /// unrelated process. Left strictly alone — 宁漏杀不误杀.
    IdentityMismatch,
    /// The process group / tree was signalled.
    Killed,
    /// Identity matched but the kill itself failed (EPERM, …).
    KillFailed,
}

/// Really kill the process trees behind this boot's orphan-settled rows.
/// Runs ONCE at reaper start, before the first sweep. Best-effort: a query
/// failure or an individual kill failure is logged, never fatal — the DB
/// settlement already happened, this is the quota-saving second half.
async fn reap_orphan_processes(store: &Store) {
    let since = now_ms() - ORPHAN_REAP_WINDOW_MS;
    let orphans = match store.orphaned_agent_processes(since).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(?e, "reaper: orphan process reap query failed");
            return;
        }
    };
    if orphans.is_empty() {
        return;
    }
    // ps/tasklist subprocesses + the SIGTERM grace sleep block the caller —
    // keep them off the async worker. Startup-only, one pass.
    let killed = tokio::task::spawn_blocking(move || {
        let mut killed = 0usize;
        for o in &orphans {
            match reap_one_orphan(o) {
                OrphanReap::Killed => {
                    killed += 1;
                    tracing::info!(
                        agent = %o.id, pid = o.shim_pid,
                        "reaper: killed orphaned agent process tree from previous server"
                    );
                }
                OrphanReap::NotRunning => {}
                OrphanReap::IdentityMismatch => tracing::info!(
                    agent = %o.id, pid = o.shim_pid,
                    "reaper: orphan pid alive but not a swarmx agent (recycled pid?); leaving it alone"
                ),
                OrphanReap::KillFailed => tracing::warn!(
                    agent = %o.id, pid = o.shim_pid,
                    "reaper: orphan identity matched but kill failed"
                ),
            }
        }
        killed
    })
    .await
    .unwrap_or(0);
    if killed > 0 {
        tracing::info!(killed, "reaper: startup orphan process reap done");
    }
}

/// Resolve a live child's process-group id. portable-pty `setsid`s the shim
/// at spawn, so this equals the pid in practice, but resolve it for real in
/// case the PTY layer ever changes. Windows has no process groups → `None`
/// (the orphan reaper fells the tree by pid via `taskkill /T` there instead).
#[cfg(unix)]
fn pgid_of(pid: u32) -> Option<i64> {
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    (pgid > 0).then_some(pgid as i64)
}

#[cfg(windows)]
fn pgid_of(_pid: u32) -> Option<i64> {
    None
}

/// unix: probe → cmdline-verify → killpg the whole tree. The stored pid is
/// the shim's; its cmdline always contains the swarmx-shim path (or, for the
/// CLI it spawned, a `.swarmx/` config path / the agent id / the CLI name).
#[cfg(unix)]
fn reap_one_orphan(o: &OrphanedAgentProcess) -> OrphanReap {
    let pid = match libc::pid_t::try_from(o.shim_pid) {
        Ok(p) if p > 0 => p,
        _ => return OrphanReap::IdentityMismatch, // corrupt row — never kill on it
    };
    if !pid_alive(pid) {
        // Residual gap, deliberately accepted: the pid probe only sees the
        // SHIM. A CLI that survived its own dead shim (shim SIGKILLed by OOM)
        // still lives in the recorded group, but enumerating group members has
        // no portable API, so that orphan leaks until reboot. 宁漏杀不误杀.
        return OrphanReap::NotRunning;
    }
    // An unreadable cmdline (EPERM after pid reuse, defunct, ps missing) is
    // treated exactly like a mismatch: we only kill a process we positively
    // identified as ours.
    let Some(cmdline) = read_cmdline(pid) else {
        return OrphanReap::IdentityMismatch;
    };
    if !cmdline_matches_agent(&cmdline, &o.id, &o.cli) {
        return OrphanReap::IdentityMismatch;
    }
    kill_agent_group(pid, o.shim_pgid)
}

/// Windows degradation: no /proc, no ps, no process groups. What we CAN
/// cheaply get is the image NAME for a pid via `tasklist` — a strong identity
/// here because the recorded pid is the shim's and the shim's image is always
/// `swarmx-shim.exe`. Anything else (or no such pid) means the process is
/// gone or the pid was recycled: leave it alone. The full cmdline would need
/// WMI/CIM, which is not worth a dependency for a startup-only sweep.
#[cfg(windows)]
fn reap_one_orphan(o: &OrphanedAgentProcess) -> OrphanReap {
    match windows_image_name(o.shim_pid) {
        None => OrphanReap::NotRunning,
        Some(name) if !name.eq_ignore_ascii_case("swarmx-shim.exe") => {
            OrphanReap::IdentityMismatch
        }
        Some(_) => {
            // taskkill /T fells the whole tree rooted at the shim (shim →
            // real CLI → its node descendants); /F forces it. Same call the
            // live-kill path in swarmx-pty uses.
            let ok = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &o.shim_pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                OrphanReap::Killed
            } else {
                OrphanReap::KillFailed
            }
        }
    }
}

/// Image name (e.g. "swarmx-shim.exe") of a live pid, or None when no such
/// process is running. Parses `tasklist` CSV; the no-match output is a
/// localized INFO line that never starts with a quote, so it is skipped
/// without parsing localized text.
#[cfg(windows)]
fn windows_image_name(pid: i64) -> Option<String> {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().find(|l| l.starts_with('"'))?;
    // "image.exe","1234","Console","1","84,000 K"
    let name = line.split('"').nth(1)?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// kill(pid, 0) liveness probe: 0 = alive and ours to signal; EPERM = alive
/// but owned by another user (possible after pid reuse — still "alive", and
/// the cmdline check below then refuses to match); ESRCH = gone.
#[cfg(unix)]
fn pid_alive(pid: libc::pid_t) -> bool {
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Full command line of a live pid. Linux `/proc` first (no subprocess);
/// macOS has no /proc, so fall back to `ps -o command= -p` (`-ww`: argv can
/// be long — a per-agent `--mcp-config …/.swarmx/mcp/<id>.json` sits deep in
/// it — and a width-truncated cmdline would silently lose the marker). None
/// when the process is gone or unreadable — the caller treats that as a
/// mismatch.
#[cfg(unix)]
fn read_cmdline(pid: libc::pid_t) -> Option<String> {
    if let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let joined = raw
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return Some(joined);
        }
        // Empty cmdline (kernel thread / zombie) → fall through to ps.
    }
    let out = crate::runtime_path::tool_command("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Does this cmdline belong to the agent row we recorded? Ordered strongest
/// first: any swarmx-owned path (the shim binary, the per-agent `.swarmx/`
/// config paths baked into argv), then the unique agent id, then the bare CLI
/// binary name as the weak-but-sanctioned fallback (the pid + recency window
/// already gate it). Empty needles are excluded — `str::contains("")` is
/// always true and would void the whole check.
#[cfg(unix)]
fn cmdline_matches_agent(cmdline: &str, agent_id: &str, cli: &str) -> bool {
    cmdline.contains("swarmx")
        || (!agent_id.is_empty() && cmdline.contains(agent_id))
        || (!cli.is_empty() && cmdline.contains(cli))
}

/// Signal the orphan's whole process group: SIGTERM (let the CLI flush) →
/// ~1s grace probed by group emptiness → SIGKILL. Mirrors `PtyBridge::kill`,
/// minus the child reap — the orphan is not our child, so init reaps it.
#[cfg(unix)]
fn kill_agent_group(pid: libc::pid_t, recorded_pgid: Option<i64>) -> OrphanReap {
    // SAFETY: getpgid/killpg on a pid we recorded; the own-group guard below
    // ensures we can never signal swarmx-server's own group.
    unsafe {
        let group = libc::getpgid(pid);
        if group <= 0 {
            return OrphanReap::NotRunning; // died between probe and now
        }
        // Cross-check the live group against what the spawn ledger recorded.
        // A mismatch means the pid was recycled by an unrelated process
        // between the crash and this boot → leave it alone.
        if let Some(recorded) = recorded_pgid {
            if recorded != group as i64 {
                return OrphanReap::IdentityMismatch;
            }
        }
        if group == libc::getpgid(0) {
            // The recorded process shares OUR process group — signalling it
            // would take down the server itself. Refuse (same guard as
            // PtyBridge::kill).
            return OrphanReap::IdentityMismatch;
        }
        libc::killpg(group, libc::SIGTERM);
        // Judge completion by the GROUP going empty (killpg(group, 0) →
        // ESRCH), not by the shim pid — the CLI may outlive its shim.
        for _ in 0..20 {
            if libc::killpg(group, 0) != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return OrphanReap::Killed;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let rc = libc::killpg(group, libc::SIGKILL);
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return OrphanReap::KillFailed;
        }
        OrphanReap::Killed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty_stream::PtyStream;
    use crate::registry::{AgentChannel, Lifecycle};
    use swarmx_pty::{PtyBridge, PtyHandles, SpawnOpts};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    fn slot_for(cmd: &str) -> Arc<Mutex<AgentSlot>> {
        Arc::new(Mutex::new(agent_slot_for(cmd)))
    }

    fn agent_slot_for(cmd: &str) -> AgentSlot {
        let PtyHandles {
            bridge,
            output_rx: _output_rx,
        } = PtyBridge::spawn(SpawnOpts {
            argv: &["/bin/sh".into(), "-c".into(), cmd.into()],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn test child");
        let input_tx = bridge.input_sender();
        let (lifecycle_tx, _rx) = tokio::sync::broadcast::channel(16);
        AgentSlot {
            channel: AgentChannel::Pty {
                bridge: Arc::new(bridge),
                stream: Arc::new(PtyStream::new()),
                input_tx,
            },
            lifecycle: Arc::new(Mutex::new(Lifecycle::default())),
            lifecycle_tx,
            cli: "test".into(),
            role: "test".into(),
            workspace: "/tmp".into(),
            paused: Arc::new(AtomicBool::new(false)),
            mcp_ready: tokio::sync::watch::channel(false).0,
            tui_http_port: None,
            serve_http_port: None,
            zulu: None,
            live_delivery: crate::input_delivery::LiveDelivery::Keystroke,
        }
    }

    async fn wait_until_dead(slot: &Mutex<AgentSlot>) {
        for _ in 0..200 {
            if !slot.lock().is_alive() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("child did not exit in time");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn detects_dead_child_and_latches_once() {
        let slot = slot_for("exit 0");
        wait_until_dead(&slot).await;

        // First sweep synthesizes the exit (code 0) and latches lifecycle.
        assert_eq!(detect_exit(&slot), Some(0));
        assert_eq!(slot.lock().lifecycle.lock().shim_exit, Some(0));
        // Second sweep is a no-op — already accounted for.
        assert_eq!(detect_exit(&slot), None);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn nonzero_exit_code_is_surfaced() {
        let slot = slot_for("exit 7");
        wait_until_dead(&slot).await;
        assert_eq!(detect_exit(&slot), Some(7));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn live_child_is_left_alone() {
        // `sleep 10` stays alive across the check.
        let slot = slot_for("sleep 10");
        assert_eq!(detect_exit(&slot), None);
        assert_eq!(slot.lock().lifecycle.lock().shim_exit, None);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn reapable_only_after_exit_is_accounted() {
        let dead = slot_for("exit 0");
        wait_until_dead(&dead).await;
        // Dead but not yet accounted (no shim_exit) → not reapable.
        assert!(!is_reapable(&dead));
        // After detect_exit latches shim_exit → reapable.
        assert_eq!(detect_exit(&dead), Some(0));
        assert!(is_reapable(&dead));
        // A live child is never reapable.
        let live = slot_for("sleep 10");
        assert!(!is_reapable(&live));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sweep_evicts_exited_slot_after_grace() {
        use swarmx_storage::Store;
        use swarmx_swarm::Swarm;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("db.sqlite")).await.unwrap());
        let swarm = Swarm::new(store.clone(), dir.path().join("bb"));
        let registry = Registry::new();
        registry.insert("a1".into(), agent_slot_for("exit 0"));
        // Wait for the child to actually die.
        if let Some(s) = registry.get("a1") {
            wait_until_dead(&s).await;
        }

        let mut exited_at = HashMap::new();
        let mut pids = HashSet::new();
        // Sweep 1 (long grace): accounts the exit, starts the clock, keeps the slot.
        sweep_once(&registry, &store, &swarm, &mut exited_at, &mut pids, Duration::from_secs(60)).await;
        assert!(registry.get("a1").is_some(), "slot kept during grace");
        // Sweep 2 (zero grace): the accounted exit is now evicted → slot + writer
        // thread + fd + live-agent-cap slot reclaimed.
        sweep_once(&registry, &store, &swarm, &mut exited_at, &mut pids, Duration::ZERO).await;
        assert!(registry.get("a1").is_none(), "exited slot evicted after grace");
        assert!(registry.list().is_empty(), "live-agent cap reclaimed");
    }

    // ── process ledger + startup orphan reap ────────────────────────────────

    fn spawn_agent_row(id: &str, cli: &str) -> swarmx_storage::NewAgent {
        swarmx_storage::NewAgent {
            id: id.into(),
            cli: cli.into(),
            role: "test".into(),
            workspace: "/tmp".into(),
            spawned_at: 0,
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        }
    }

    async fn test_store(dir: &tempfile::TempDir) -> Arc<Store> {
        Arc::new(Store::open(&dir.path().join("db.sqlite")).await.unwrap())
    }

    /// Reap our own test child while a group kill is in flight. In production
    /// the orphan's shim is reparented to init, which reaps it the moment it
    /// dies; in a test the dead child is OURS and would otherwise sit as a
    /// zombie in its process group — `killpg(group, 0)` counts zombies, so the
    /// group would look non-empty for the whole grace and derail the probe
    /// into the SIGKILL path (which macOS then answers with EPERM).
    fn zombie_reaper_for(slot: &Arc<Mutex<AgentSlot>>) -> std::thread::JoinHandle<()> {
        let slot = slot.clone();
        std::thread::spawn(move || {
            for _ in 0..100 {
                // is_alive() → try_wait() → reaps the zombie.
                if !slot.lock().is_alive() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sweep_persists_shim_pid_and_pgid_once() {
        use swarmx_swarm::Swarm;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(&dir).await;
        store
            .record_agent_spawn(spawn_agent_row("a1", "sleep"))
            .await
            .unwrap();
        let swarm = Swarm::new(store.clone(), dir.path().join("bb"));
        let registry = Registry::new();
        registry.insert("a1".into(), agent_slot_for("sleep 10"));
        let expected_pid = registry.get("a1").unwrap().lock().shim_pid().unwrap();

        let mut exited_at = HashMap::new();
        let mut pids = HashSet::new();
        sweep_once(&registry, &store, &swarm, &mut exited_at, &mut pids, Duration::ZERO).await;
        assert!(pids.contains("a1"), "pid recorded on first sweep");

        // Read back through the orphan-candidate query (the only consumer):
        // settle the row as killed inside the window and confirm the pid/pgid
        // the next boot would reap from. portable-pty setsid's the shim, so
        // pgid == pid (pinned by swarmx-pty's own kill test too).
        store.record_agent_kill("a1".into(), now_ms()).await.unwrap();
        let cands = store.orphaned_agent_processes(now_ms() - 1000).await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].shim_pid, expected_pid as i64);
        assert_eq!(cands[0].shim_pgid, Some(expected_pid as i64));

        // A second sweep must not rewrite (first-write-wins in the store AND
        // the in-memory set skips the write entirely).
        sweep_once(&registry, &store, &swarm, &mut exited_at, &mut pids, Duration::ZERO).await;

        // Clean up our own test child.
        if let Some(s) = registry.get("a1") {
            s.lock().kill();
        }
    }

    #[test]
    #[cfg(unix)]
    fn cmdline_match_requires_swarmx_marker_or_agent_id_or_cli() {
        // The shim's own cmdline always carries its path → "swarmx".
        assert!(cmdline_matches_agent(
            "/usr/local/bin/swarmx-shim /opt/homebrew/bin/claude --model opus",
            "claude-a1b2c3d4",
            "claude"
        ));
        // The real CLI's argv carries per-agent .swarmx config paths / the id.
        assert!(cmdline_matches_agent(
            "node /opt/homebrew/bin/claude --mcp-config /Users/x/.swarmx/mcp/claude-a1b2c3d4.json",
            "claude-a1b2c3d4",
            "claude"
        ));
        // Weak fallback: the bare CLI name (gated by pid + recency window).
        assert!(cmdline_matches_agent(
            "node /opt/homebrew/bin/claude --model opus",
            "claude-a1b2c3d4",
            "claude"
        ));
        // An unrelated process (pid recycled) must NOT match.
        assert!(!cmdline_matches_agent("vim main.rs", "claude-a1b2c3d4", "claude"));
        assert!(!cmdline_matches_agent("", "claude-a1b2c3d4", "claude"));
        // Empty needles can never match (str::contains("") is true).
        assert!(!cmdline_matches_agent("anything at all", "", ""));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn orphan_reap_kills_real_process_group() {
        // A "previous server" orphan: a live, setsid'd process group
        // (/bin/sh → sleep) whose row carries pid+pgid and whose cmdline
        // ("/bin/sh -c sleep 60") matches the recorded cli name.
        let slot = slot_for("sleep 60");
        let pid = slot.lock().shim_pid().expect("child pid");
        let orphan = OrphanedAgentProcess {
            id: "sleep-orch1".into(), // deliberately NOT in the cmdline
            cli: "sleep".into(),
            shim_pid: pid as i64,
            shim_pgid: Some(pid as i64),
        };
        let zr = zombie_reaper_for(&slot);
        assert_eq!(reap_one_orphan(&orphan), OrphanReap::Killed);
        zr.join().unwrap();
        assert!(!slot.lock().is_alive(), "shim dead after group kill");
        let mut group_gone = false;
        for _ in 0..40 {
            let rc = unsafe { libc::killpg(pid as libc::pid_t, 0) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                group_gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(group_gone, "whole orphan process group must be dead");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn orphan_reap_refuses_cmdline_mismatch() {
        // Same live process group, but the recorded identity matches nothing
        // in its cmdline → the pid must have been recycled; leave it running.
        let slot = slot_for("sleep 60");
        let pid = slot.lock().shim_pid().expect("child pid");
        let orphan = OrphanedAgentProcess {
            id: "zzz-nomatch".into(),
            cli: "definitely-not-matching".into(),
            shim_pid: pid as i64,
            shim_pgid: Some(pid as i64),
        };
        assert_eq!(reap_one_orphan(&orphan), OrphanReap::IdentityMismatch);
        assert!(
            slot.lock().is_alive(),
            "a mismatched (recycled) pid must be left strictly alone"
        );
        // Clean up our own test child.
        slot.lock().kill();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn orphan_reap_refuses_pgid_mismatch() {
        // cmdline matches but the recorded pgid disagrees with the live one —
        // another recycled-pid shape that must not be killed.
        let slot = slot_for("sleep 60");
        let pid = slot.lock().shim_pid().expect("child pid");
        let orphan = OrphanedAgentProcess {
            id: "sleep-orch2".into(),
            cli: "sleep".into(),
            shim_pid: pid as i64,
            shim_pgid: Some(pid as i64 + 10_000), // wrong on purpose
        };
        assert_eq!(reap_one_orphan(&orphan), OrphanReap::IdentityMismatch);
        assert!(slot.lock().is_alive());
        slot.lock().kill();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn orphan_reap_skips_already_dead_pid() {
        let slot = slot_for("exit 0");
        let pid = slot.lock().shim_pid().expect("child pid");
        wait_until_dead(&slot).await;
        let orphan = OrphanedAgentProcess {
            id: "sleep-gone".into(),
            cli: "sh".into(),
            shim_pid: pid as i64,
            shim_pgid: Some(pid as i64),
        };
        assert_eq!(reap_one_orphan(&orphan), OrphanReap::NotRunning);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn startup_reap_kills_process_behind_boot_settled_row() {
        // End-to-end through the store: row spawned + pid recorded + settled
        // by the boot sweep (mark_orphan_agents_killed) → the real process
        // group dies, not just the DB row.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(&dir).await;
        let slot = slot_for("sleep 60");
        let pid = slot.lock().shim_pid().expect("child pid");
        store
            .record_agent_spawn(spawn_agent_row("sleep-zz1", "sleep"))
            .await
            .unwrap();
        assert!(
            store
                .record_agent_shim_pid("sleep-zz1".into(), pid as i64, Some(pid as i64))
                .await
                .unwrap()
        );
        store.mark_orphan_agents_killed(now_ms()).await.unwrap();

        let zr = zombie_reaper_for(&slot);
        reap_orphan_processes(&store).await;
        zr.join().unwrap();

        assert!(
            !slot.lock().is_alive(),
            "boot-settled orphan's process must really be dead"
        );
    }
}
