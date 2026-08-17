//! M6b WakeCoordinator: wake agents the moment a blackboard key they
//! declared `depends_on` is written.
//!
//! The gap this closes (observed in M6a run #3): when an agent finishes
//! a turn with an empty mailbox and its prompt told it to wait for some
//! `*.done` key, the Stop hook noop's, the agent sits idle, and later
//! writes to that key never resurrect it. wake-check is a Stop *hook* —
//! it only fires when an agent is in the act of stopping; it cannot
//! restart an already-stopped one.
//!
//! Design (validated against the 2025 blackboard-architecture revival
//! papers, Han et al. arXiv 2507.01701 and Salemi et al. arXiv
//! 2510.01285): the orchestrator owns wakeup. A single tokio task
//! subscribes to `Swarm`'s broadcast channel, watches for
//! `SwarmEvent::BlackboardChanged`, and for each subscribed agent does
//! two things:
//!
//!   1. **Mailbox write** (source of truth): `Swarm::send_message` posts
//!      a `kind="wake"` note from `"system"` to the agent. Even if the
//!      PTY kick below fails, the next time the agent stops, wake-check
//!      will see this unread note and force it to keep going. On a
//!      successful kick the note is instead consumed at delivery time,
//!      so the trailing wake-check sees 0 and noops — one wake = ONE
//!      turn, not two (see `kick_agent`). Idempotent.
//!
//!   2. **PTY kick** (belt-and-suspenders): byte-blast `\x15<short>\r`
//!      into the agent's PTY input channel. Ctrl-U clears any residual
//!      text in the TUI's input buffer; the short text + carriage return
//!      submits a fresh user turn so the agent does NOT have to wait for
//!      the next natural Stop event. Best-effort: failure is logged and
//!      not propagated.
//!
//! The writer is excluded from the wakeup set so BE doesn't wake itself
//! the instant it writes its own `backend.done`. External edits
//! (`agent_id: None`) wake everyone subscribed.

use anyhow::{anyhow, Result};
#[allow(unused_imports)] // used by wake tests' live_pty_slot signatures
use bytes::Bytes;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use swarmx_protocol::ws_swarm::SwarmEvent;
use swarmx_swarm::{NewMessage, Swarm};
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinHandle;

use crate::registry::Registry;

/// Per-agent dependency table. The wake task reads this on every
/// blackboard event; spell launch writes to it once per agent.
///
/// Per-agent (agent_id → keys) rather than inverted (key → Vec<agent_id>)
/// because cleanup on agent kill is O(1) — the common path. Lookup on
/// event is a linear scan over ≤ ~10 entries per spell, which is fine.
pub type WakeSubs = Arc<RwLock<HashMap<String, Vec<String>>>>;

/// M6c step 5: per-agent expected handoff-signal + spawn time. When
/// the agent exits without writing its handoff_signal we synthesize a
/// `<signal>.error` so downstream dependents can fail loudly instead
/// of hanging. The spawn timestamp lets us distinguish a fresh write
/// (this run's agent succeeded) from a stale leftover on disk (a
/// previous run's `<signal>` row still in the blackboard) so we don't
/// silently skip writing `.error` because yesterday's run happened to
/// produce the same key name.
///
/// Only agents whose role declares a non-empty `handoff_signal` are
/// registered — inline-only roles (critic-loop's writer / critic /
/// editor) don't get exit-fallback because there's no canonical signal
/// to mark as failed.
/// How long to wait after a worker writes its `handoff_signal` before
/// the auto-kill fires. 5 seconds is long enough for claude/codex to
/// finish printing the final scrollback + a `swarm_send_message`
/// summary back to the orchestrator (typically <2s), but short enough
/// that the UI ground-truth converges quickly. Tune up if recording
/// playback shows truncation; tune down if zombie PTYs feel sluggish.
const AUTO_KILL_GRACE_MS: u64 = 5_000;

#[derive(Debug, Clone)]
pub struct ExitKey {
    /// Role name — used to name the synthesized failure key as
    /// `<role>.error` (matches the convention agents already use when
    /// they self-write a failure, e.g. `frontend.error`, so test agent
    /// prompts only need to check ONE key name regardless of whether
    /// FE crashed or self-aborted).
    pub role: String,
    /// Blackboard key the role was supposed to produce. Used (a) for
    /// the freshness check ("did the agent actually write this before
    /// dying?") and (b) to identify which agents to wake when we
    /// synthesize the error — we wake subscribers of THIS key, not of
    /// `<role>.error`, because that's what `depends_on` actually lists.
    pub handoff_signal: String,
    /// When the registration was made. A blackboard write to
    /// `handoff_signal` older than this is a leftover from a previous
    /// run on the same workspace and must NOT short-circuit the
    /// .error synthesis.
    pub spawned_at_ms: i64,
    /// W2-1 verify gate (opt-in): objective checks the server runs in the
    /// agent's cwd when it writes `handoff_signal`, BEFORE the completion is
    /// accepted (auto-kill). Empty (the default) = no gate — the legacy
    /// "handoff write == done" behaviour is unchanged. Populated at spawn
    /// time from the role's `done_checks`, already allowlist-validated.
    pub verify_cmds: Vec<String>,
    /// Consecutive verify-gate failures already bounced back to this worker.
    /// In-memory only (dies with the registration on kill/exit, resets on
    /// restart) — it exists purely to bound the fix loop's token burn; see
    /// `MAX_VERIFY_BOUNCES`.
    pub verify_attempts: u32,
}
pub type ExitKeys = Arc<RwLock<HashMap<String, ExitKey>>>;

/// W2-1 dead-loop guardrail (the design doc marks this 必抄): how many times
/// a failed verify gate bounces the delivery back to the worker for a fix
/// before the server stops bouncing. Precedents: Claude Code force-allows
/// after 8 consecutive hook blocks, Codex caps its fix loop at 3; the W2-1
/// design doc picks 2. Past the cap the worker is told to escalate (write
/// `<key>.error` / report to the orchestrator) — the server never silently
/// marks failed work done, and never loops forever burning tokens.
const MAX_VERIFY_BOUNCES: u32 = 2;

/// Recognises blackboard keys that should fan-out to wake the base
/// key's subscribers in addition to their literal name. Today only
/// `.error` and `.failed` suffixes get this treatment — both indicate
/// "the producer for the base key isn't coming". An empty Vec means
/// "no fan-out, treat as a regular key".
fn base_key_aliases(path: &str) -> Vec<String> {
    for suffix in [".error", ".failed"] {
        if let Some(base) = path.strip_suffix(suffix) {
            if !base.is_empty() {
                return vec![base.to_string()];
            }
        }
    }
    Vec::new()
}

/// Mailbox / kick prose for a blackboard wake. Failure aliases (`.error` /
/// `.failed`) are not "an update" — they are the replan signal: same-role
/// replacement spawn is allowed because Handoff mints a new instance key.
pub fn wake_mailbox_body(key: &str) -> String {
    if let Some(base) = base_key_aliases(key).into_iter().next() {
        format!(
            "失败：`{key}`（上游 `{base}` 没交付就退出了）。等待方走失败路径，不要空等变绿。\
             规划可再 swarm_spawn_worker 同角色顶上一次——server 会 mint 新 instance key，\
             不会覆盖已有产物。先读 `{key}` 看谁挂了。"
        )
    } else {
        format!("共享区 `{key}` 有更新，请查看")
    }
}

/// Inserts `agent_id → keys` into the subscription table. No-op when
/// `keys` is empty (we don't bother storing zero-dep agents).
pub async fn register_wake_subs(subs: &WakeSubs, agent_id: String, keys: Vec<String>) {
    if keys.is_empty() {
        return;
    }
    let mut w = subs.write().await;
    w.insert(agent_id, keys);
}

/// Append a single key to `agent_id`'s subscription list, creating the
/// entry if it doesn't exist. Idempotent — a duplicate key is silently
/// dropped. Used by `spawn_worker` to make the spawning agent (= the
/// Magentic-One orchestrator) subscribe to the new worker's
/// `handoff_signal` without clobbering any subscriptions the
/// orchestrator already had from prior spawns.
pub async fn append_wake_sub(subs: &WakeSubs, agent_id: String, key: String) {
    if key.is_empty() {
        return;
    }
    let mut w = subs.write().await;
    let entry = w.entry(agent_id).or_default();
    if !entry.contains(&key) {
        entry.push(key);
    }
}

/// Removes an agent's subscription. Called from the kill handler so
/// blackboard writes to dead agents' depended-on keys don't try to wake
/// a registry slot that has been dropped.
pub async fn unregister_wake_subs(subs: &WakeSubs, agent_id: &str) {
    let mut w = subs.write().await;
    w.remove(agent_id);
}

/// Insert this agent's expected handoff_signal + spawn time. No-op when
/// the signal is empty (inline-only roles, planner, etc.). Called from
/// `run_spell` alongside `register_wake_subs`.
///
/// `verify_cmds` is the W2-1 verify gate: the role's `done_checks`, already
/// allowlist-validated at spawn time. Pass an empty Vec (the spell path
/// does) for the legacy no-gate behaviour.
pub async fn register_exit_key(
    keys: &ExitKeys,
    agent_id: String,
    role: String,
    handoff_signal: String,
    spawned_at_ms: i64,
    verify_cmds: Vec<String>,
) {
    if handoff_signal.is_empty() {
        return;
    }
    let mut w = keys.write().await;
    w.insert(
        agent_id,
        ExitKey {
            role,
            handoff_signal,
            spawned_at_ms,
            verify_cmds,
            verify_attempts: 0,
        },
    );
}

/// Remove this agent's exit-key registration. Called from the kill
/// handler before the registry slot is dropped — symmetric with
/// `unregister_wake_subs`.
pub async fn unregister_exit_key(keys: &ExitKeys, agent_id: &str) {
    let mut w = keys.write().await;
    w.remove(agent_id);
}

/// Pure function (no IO, no async) extracted for unit testing: given a
/// snapshot of the subscription table, the just-written key, and the
/// writer (if any), produce the list of agent_ids to wake. Writer is
/// excluded by design — `BE writes backend.done` should not wake BE.
pub fn select_targets(
    subs: &HashMap<String, Vec<String>>,
    key: &str,
    writer: Option<&str>,
) -> Vec<String> {
    subs.iter()
        .filter(|(aid, keys)| {
            // Skip the writer itself; tooling that legitimately watches
            // its own key would create wake-storms otherwise.
            if writer.is_some_and(|w| w == aid.as_str()) {
                return false;
            }
            keys.iter().any(|k| k == key)
        })
        .map(|(aid, _)| aid.clone())
        .collect()
}

/// Pure (no IO/async) extracted for unit testing: pick the agents to auto-kill
/// after a handoff write. An agent is reaped ONLY when it produced its OWN
/// declared `handoff_signal` — i.e. the written `path` equals its
/// `handoff_signal` AND it is the `writer` (F13). Without the writer guard, two
/// agents that happen to declare the same `handoff_signal` would BOTH be killed
/// when either writes it, silently reaping a sibling that hasn't finished. An
/// unattributed write (`writer = None`, e.g. external editor / reconcile)
/// reaps no one. Returns `(agent_id, role)` pairs.
pub fn select_autokill_targets(
    exit_keys: &HashMap<String, ExitKey>,
    path: &str,
    writer: Option<&str>,
) -> Vec<(String, String)> {
    exit_keys
        .iter()
        .filter(|(aid, ek)| ek.handoff_signal == path && writer == Some(aid.as_str()))
        .map(|(aid, ek)| (aid.clone(), ek.role.clone()))
        .collect()
}

/// Diagnose the dominant silent-stall failure mode: a blackboard write that
/// IS some agent's declared `handoff_signal` yet matched ZERO `depends_on`
/// subscribers. That means a producer just shipped its completion key but
/// nothing is wired to react — almost always a key-string mismatch between the
/// producer's `handoff_signal` and a dependent's `depends_on` (a missing
/// `<workspace_id>` prefix, a trailing slash, or a typo). Wake matching is
/// exact-string, so the dependent then hangs forever with no other signal.
///
/// Returns `Some(keys_other_agents_are_waiting_on)` when this is an orphaned
/// handoff (the caller logs a warning with that context so the mismatch is
/// visible), or `None` otherwise. Pure (no IO/async) for unit testing.
/// `woke_anyone` = whether the fan-out already delivered to ≥1 subscriber.
pub fn orphaned_handoff_diagnosis(
    subs: &HashMap<String, Vec<String>>,
    handoff_signals: &[String],
    written_key: &str,
    woke_anyone: bool,
) -> Option<Vec<String>> {
    if woke_anyone {
        return None;
    }
    if !handoff_signals.iter().any(|h| h == written_key) {
        return None;
    }
    let mut waiting: Vec<String> = subs.values().flatten().cloned().collect();
    waiting.sort();
    waiting.dedup();
    Some(waiting)
}

/// Lag recovery (F12): the shared SwarmEvent broadcast can drop events under a
/// burst, and a dropped `BlackboardChanged` for a one-shot handoff key is lost
/// forever — there's no "next write" to catch up, so the dependent hangs and
/// no mailbox wake row is ever written either. On `Lagged` the coordinator
/// reconciles: given the subs snapshot and the set of `depends_on` keys we've
/// confirmed are already present on the blackboard (`satisfied`), return one
/// `(agent, key)` to re-wake per affected agent. Re-waking is benign (a
/// redundant recheck turn) and far better than a permanent stall. Pure +
/// deterministically ordered for unit testing.
pub fn agents_to_rewake(
    subs: &HashMap<String, Vec<String>>,
    satisfied: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = subs
        .iter()
        .filter_map(|(agent, keys)| {
            keys.iter()
                .find(|k| satisfied.contains(k.as_str()))
                .map(|k| (agent.clone(), k.clone()))
        })
        .collect();
    out.sort();
    out
}

/// Blackboard-key wake kick (auto path). Routes through
/// [`crate::input_delivery::deliver_wake_turn`] — every engine including
/// reasonix. Callers that need a custom body use `inject_with_kick_text`.
#[allow(dead_code)] // exercised by unit tests; kick_agent calls TurnDelivery directly
pub async fn inject_wake_kick(
    registry: &Registry,
    server_url: &str,
    agent_id: &str,
    key: &str,
) -> Result<()> {
    let kick_text = wake_mailbox_body(key);
    inject_with_kick_text(registry, server_url, agent_id, &kick_text, key, None).await
}

/// Wake kick with a custom body. All engines (incl. reasonix) go through
/// TurnDelivery — there is no longer a "reasonix must not be handled here"
/// Err path. `reasonix_body` is forwarded when Some (manual / verify);
/// None uses the generic reasonix wake recipe.
#[allow(dead_code)] // thin wrapper over TurnDelivery; keep for call-site clarity / tests
pub async fn inject_with_kick_text(
    registry: &Registry,
    server_url: &str,
    agent_id: &str,
    kick_text: &str,
    key_for_log: &str,
    reasonix_body: Option<&str>,
) -> Result<()> {
    crate::input_delivery::deliver_wake_turn(
        registry,
        agent_id,
        kick_text,
        key_for_log,
        crate::input_delivery::WakeTurnCtx {
            server_url,
            reasonix_body,
        },
    )
    .await
    .map(|_| ())
}

/// M6e: operator-triggered manual wake. Same delivery shape as the
/// BlackboardChanged-driven wake (mailbox `kind=wake` + PTY kick), but
/// the kick text says "manual wake from operator" instead of pretending
/// there's a key update. Used by the UI's ⚡ button when an operator
/// believes an agent has missed a wake or is stuck.
///
/// Mailbox is the source of truth; if it fails we bail (sending a PTY
/// kick with no context would be misleading). The PTY kick itself is
/// best-effort — failure usually means the agent has exited, in which
/// case the mailbox entry is also moot but we've already returned Ok
/// (caller wanted a fire-and-forget signal, not a delivery guarantee).
pub async fn deliver_manual_wake(
    swarm: &Swarm,
    registry: &Registry,
    server_url: &str,
    target: &str,
) -> Result<()> {
    let body =
        "操作员唤醒——请先查收邮箱里的新消息（可能是用户的新指令），再检查共享区，然后继续。\
         如果读到需要回复用户的消息，必须调用 swarm_send_message(to=\"user\", kind=\"reply\", body=...) \
         把回复发回 swarmx 聊天；不要只在你自己的 final answer 里结束。";
    // Operator-initiated wake → keep it visible in the feed (a real
    // intervention worth recording), distinct from the high-volume
    // auto blackboard wakes the UI filters out.
    let meta = serde_json::json!({ "subtype": "wake", "reason": "manual" });
    deliver_wake_with_body(
        swarm,
        registry,
        server_url,
        target,
        body,
        meta,
        "manual wake",
    )
    .await
}

/// S5 stuck-watchdog wake (see `stuck_watchdog.rs`). Same delivery shape as
/// the operator ⚡ (`deliver_manual_wake`): the mailbox `kind=wake` row is
/// the source of truth, the engine kick via TurnDelivery is best-effort. The
/// prose names the owed handoff key and says plainly the system will NOT kill
/// the agent — a suspected-stuck agent must never read its own obituary, and
/// an honest "prove you're alive" beats a vague poke (the M6d TTL scanner was
/// removed because naked nudges pushed agents into fabricating handoffs).
pub async fn deliver_watchdog_wake(
    swarm: &Swarm,
    registry: &Registry,
    server_url: &str,
    target: &str,
    handoff_key: &str,
    silence_min: u64,
) -> Result<()> {
    let body = format!(
        "系统看门狗：你的进程还活着，但已超过 {silence_min} 分钟没有任何活动迹象\
         （无工具调用 / 消息 / token 用量 / 黑板写入），而你的交付键 `{handoff_key}` 还没写。\
         请查收邮箱和共享区，继续推进并写出交付键；如果你真的被卡住（授权弹窗 / 网络 / 等待输入），\
         用 swarm_send_message(to=\"user\", kind=\"reply\", body=…) 说明卡点。\
         这只是提醒——系统不会杀你。"
    );
    let meta = serde_json::json!({ "subtype": "wake", "reason": "watchdog" });
    deliver_wake_with_body(
        swarm,
        registry,
        server_url,
        target,
        &body,
        meta,
        "watchdog wake",
    )
    .await
}

/// Shared delivery engine behind `deliver_manual_wake` and the W2-1 verify
/// gate's bounce-back: mailbox `kind="wake"` row (source of truth) +
/// engine-routed kick. Every branch that successfully starts a turn also
/// consumes the wake row(s) it delivered, so the trailing Stop hook
/// (wake-check) sees count=0 and noops — one wake = ONE turn, not two
/// (mirrors `kick_agent`'s per-engine consume semantics).
///
/// Mailbox is the source of truth; if it fails we bail (sending a PTY
/// kick with no context would be misleading). The kick itself is
/// best-effort — failure usually means the agent has exited, in which
/// case the mailbox entry is also moot but we've already returned Ok
/// (caller wanted a fire-and-forget signal, not a delivery guarantee).
///
/// `label` distinguishes the caller in log lines ("manual wake" / "verify
/// bounce").
#[allow(clippy::too_many_arguments)]
async fn deliver_wake_with_body(
    swarm: &Swarm,
    registry: &Registry,
    server_url: &str,
    target: &str,
    body: &str,
    meta: serde_json::Value,
    label: &str,
) -> Result<()> {
    let now = now_ms();
    let msg = NewMessage {
        from_agent: "system".into(),
        to_agent: target.into(),
        kind: "wake".into(),
        body: body.into(),
        sent_at: now,
        in_reply_to: None,
        meta: Some(meta),
    };
    swarm
        .send_message(msg)
        .await
        .map_err(|e| anyhow!("{label} mailbox send failed: {e}"))?;
    // Single-flight the kick against the SHARED global lock so this wake and
    // a BlackboardChanged auto-kick can't concurrently kick the same agent
    // (opencode double `deliver_turn` / interleaved PTY inject). If a kick is
    // already in flight, skip: the mailbox note we wrote above is covered by that
    // kick's turn / the engine's post-turn drain / the next Stop hook.
    let lock = kick_lock_for(target);
    let Ok(_kick) = lock.try_lock() else {
        tracing::debug!(
            target,
            label,
            "wake coalesced with an in-flight kick (mailbox delivered)"
        );
        return Ok(());
    };

    // Pre-consume for opencode so swarmx-wake.js does not double-kick after
    // deliver_turn. Reasonix/zulu consume atomically inside wake_if_idle.
    let channel_hint = match registry.get(target) {
        Some(slot) => crate::input_delivery::LiveDelivery::classify(&slot.lock()).kind_name(),
        None => "",
    };
    if channel_hint == "opencode-tui-http" {
        if let Err(err) = crate::wake_claim::claim(swarm, target, now).await {
            tracing::warn!(
                ?err,
                target,
                label,
                "opencode pre-consume wakes failed; plugin may double-kick"
            );
        }
    }

    match crate::input_delivery::deliver_wake_turn(
        registry,
        target,
        body,
        label,
        crate::input_delivery::WakeTurnCtx {
            server_url,
            reasonix_body: Some(body),
        },
    )
    .await
    {
        Ok(crate::input_delivery::WakeChannel::Keystroke) => {
            // PTY inject started a turn — consume wake rows so Stop hook noops.
            if let Err(err) = crate::wake_claim::claim(swarm, target, now).await {
                tracing::warn!(
                    ?err,
                    target,
                    label,
                    "wake post-inject consume_wakes failed; Stop hook may double-kick"
                );
            }
        }
        Ok(ch) => {
            tracing::debug!(target, label, channel = ?ch, "wake delivered via TurnDelivery");
        }
        Err(err) => {
            tracing::debug!(
                ?err,
                target,
                label,
                "wake inject failed (mailbox delivered, will catch on next Stop)"
            );
        }
    }
    tracing::info!(target, label, "wake delivered");
    Ok(())
}

pub struct WakeCoordinator {
    swarm: Arc<Swarm>,
    registry: Registry,
    subs: WakeSubs,
    exit_keys: ExitKeys,
    /// Needed for the post-handoff auto-kill path: when a worker writes
    /// its handoff_signal we mark its DB row as killed too, otherwise
    /// the agent stays "live" forever in `list_agents`.
    store: Arc<swarmx_storage::Store>,
    /// This server's own base URL (loopback). Used to drive reasonix agents over
    /// their `reasonix serve` HTTP API (consume_wakes + /submit) on the idle-wake
    /// path — reasonix has no PTY to kick. See `crate::reasonix_serve`.
    server_url: String,
    /// Bounds total in-flight wake kicks so a burst can't spawn unboundedly.
    delivery_sem: Arc<Semaphore>,
}

/// Max concurrent wake kicks. Comfortably above the handful of agents a single
/// blackboard write fans out to, low enough to bound resource use under a burst.
const MAX_CONCURRENT_KICKS: usize = 32;

/// Process-wide per-agent kick single-flight, shared by BOTH the auto
/// (BlackboardChanged) and manual (operator ⚡ / cron / external-message) wake
/// paths — so neither can concurrently kick the same agent (opencode's
/// unconditional `deliver_turn` would double-submit; two PTY injects would
/// interleave). Different agents kick concurrently.
///
/// Entries are NEVER removed mid-run: the single-flight guarantee depends on
/// every kick for an agent seeing the SAME `Arc<Mutex>`. Removing one while a
/// kick is in flight would let the next kick mint a fresh mutex and run
/// concurrently. The map grows by one tiny `Arc<Mutex<()>>` per distinct agent
/// woken, bounded over a process lifetime and cleared on restart — negligible.
fn kick_locks() -> &'static DashMap<String, Arc<tokio::sync::Mutex<()>>> {
    static LOCKS: std::sync::OnceLock<DashMap<String, Arc<tokio::sync::Mutex<()>>>> =
        std::sync::OnceLock::new();
    LOCKS.get_or_init(DashMap::new)
}

/// The single-flight lock for `agent_id` (get-or-create; never removed). Clone
/// the `Arc` out from under the DashMap shard lock — never hold that across an
/// `.await`.
fn kick_lock_for(agent_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    kick_locks()
        .entry(agent_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .value()
        .clone()
}

impl WakeCoordinator {
    /// Spawns the wake task and returns its JoinHandle. The handle is
    /// dropped immediately by `main.rs` since the task runs for the
    /// lifetime of the process (it exits only when the broadcast channel
    /// closes, which happens at server shutdown).
    pub fn spawn(
        swarm: Arc<Swarm>,
        registry: Registry,
        subs: WakeSubs,
        exit_keys: ExitKeys,
        store: Arc<swarmx_storage::Store>,
        server_url: String,
    ) -> JoinHandle<()> {
        let me = Self {
            swarm,
            registry,
            subs,
            exit_keys,
            store,
            server_url,
            delivery_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_KICKS)),
        };
        tokio::spawn(me.run())
    }

    /// Run loop: subscribe to swarm broadcast and react to two event
    /// kinds — BlackboardChanged (wake subscribers of the written key)
    /// and AgentState::Exited (M6c-5 .error fallback for producer death).
    ///
    /// Note (M6e, 2026-05-23): the earlier M6d-5/5b/5c TTL scanner was
    /// removed after 5 e2e runs + research across 4 sibling projects
    /// (golutra, swarm-ide, openclaw, hermes-agent) showed the design
    /// was structurally wrong: "PTY quiet for N min" is a transient
    /// observation, not a stable property (Chandy-Lamport 1985), and
    /// nudging a producer mid-stream demonstrably caused LLM agents to
    /// fabricate handoff signals (MAST FM-3.1 "Premature Termination",
    /// arXiv 2503.13657). The blackboard event + M6c-5 .error fallback
    /// together cover every observed failure mode; truly stuck agents
    /// are surfaced through the UI's manual ⚡ wake button (operator
    /// hatch, modeled after swarm-ide's stop-all and openclaw's
    /// `doctor --fix`).
    async fn run(self) {
        use tokio::sync::broadcast::error::RecvError;
        let mut rx = self.swarm.subscribe();
        loop {
            match rx.recv().await {
                Ok(SwarmEvent::BlackboardChanged {
                    agent_id: writer,
                    path,
                    ..
                }) => {
                    // Build the set of keys this write should fan out to.
                    // For a `<X>.error` or `<X>.failed` write, base key
                    // subscribers (agents that depend_on `<X>`) also get
                    // woken — that's the M6c step 5 "producer died, give
                    // up" path. Their role prompts already check for
                    // .error/.failed and branch accordingly.
                    let mut keys_to_fan: Vec<String> = vec![path.clone()];
                    keys_to_fan.extend(base_key_aliases(&path));

                    // Snapshot subs once; iterate fan-out keys against it.
                    let map = self.subs.read().await.clone();
                    // De-dup targets across fan-out keys so an agent
                    // doesn't get N redundant kicks if it happens to
                    // subscribe to both `<X>` and `<X>.error`.
                    let mut delivered: std::collections::HashSet<String> = Default::default();
                    for key in &keys_to_fan {
                        for t in select_targets(&map, key, writer.as_deref()) {
                            if delivered.insert(t.clone()) {
                                self.spawn_deliver_wake(t, path.clone());
                            }
                        }
                    }

                    // Diagnose the dominant silent stall (F3): nobody was woken
                    // by this write. If the key IS some agent's declared
                    // handoff_signal, a producer just "finished" but no
                    // dependent is wired to it — a depends_on/handoff_signal key
                    // mismatch that would otherwise hang the dependent with zero
                    // diagnostics. Only read exit_keys on this rare zero-wake
                    // path, so the common (matched) case stays cheap.
                    if delivered.is_empty() {
                        let handoffs: Vec<String> = {
                            let ek = self.exit_keys.read().await;
                            ek.values().map(|e| e.handoff_signal.clone()).collect()
                        };
                        if let Some(waiting) =
                            orphaned_handoff_diagnosis(&map, &handoffs, &path, false)
                        {
                            tracing::warn!(
                                handoff = %path,
                                waiting_on = ?waiting,
                                "handoff signal written but NO agent depends_on it — likely a \
                                 depends_on/handoff_signal key mismatch; the dependent will hang \
                                 forever. Verify the keys match EXACTLY (workspace_id prefix, \
                                 trailing slash, spelling)."
                            );
                        }
                    }

                    // Post-handoff auto-kill: if this blackboard write
                    // matches some agent's registered handoff_signal,
                    // that worker has done its job. claude/codex CLIs
                    // don't self-exit after STOPping a reply — their
                    // PTY sits idle forever, leaking process + per-agent
                    // MCP config + a phantom "alive" row in the UI. We
                    // tear that down on a small grace delay (let the
                    // worker finish printing its final scroll, let the
                    // recording flush) so the agent list and DAG return
                    // to ground truth without operator action.
                    self.maybe_auto_kill_on_handoff(&path, writer.as_deref())
                        .await;
                }
                Ok(SwarmEvent::AgentState { agent_id, state }) => {
                    // A producer that DIES must fail-loud to its downstream —
                    // whether the exit was clean (`Exited`) or abnormal (`Error`).
                    // The reaper synthesizes `Error` for a non-zero / crashed /
                    // SIGKILLed child that left no ShimExit marker (reaper.rs);
                    // before this, only `Exited` reached `handle_agent_exit`, so a
                    // crashed producer never wrote `<signal>.error` and every
                    // dependent hung at the readiness gate for MAX_WAIT (300s).
                    // Both states now route to `handle_agent_exit`, which is
                    // idempotent (it unregisters the exit_key on first call), so a
                    // death that emits Error AND a later event writes `.error` once.
                    if matches!(
                        state,
                        swarmx_protocol::ws_swarm::AgentState::Exited
                            | swarmx_protocol::ws_swarm::AgentState::Error
                    ) {
                        self.handle_agent_exit(&agent_id).await;
                    }
                }
                Ok(_) => {} // ignore the rest (message, message_read)
                Err(RecvError::Lagged(n)) => {
                    // Broadcast overflow: the coordinator fell behind a burst
                    // and the ring dropped events. A dropped BlackboardChanged
                    // for a one-shot handoff key has NO "next write" to catch
                    // up (and no mailbox row was written), so the dependent
                    // would hang forever (F12). Recover by reconciling every
                    // depends_on against the blackboard and re-waking anything
                    // already satisfied.
                    tracing::warn!(
                        lagged = n,
                        "wake coordinator broadcast lagged; reconciling depends_on against the blackboard"
                    );
                    self.reconcile_after_lag().await;
                }
                Err(RecvError::Closed) => {
                    tracing::info!("wake coordinator: broadcast closed, exiting");
                    break;
                }
            }
        }
    }

    /// Auto-kill a worker that just produced its `handoff_signal`.
    /// Reverse-scan `exit_keys` for any agent whose `handoff_signal`
    /// matches `path`. claude/codex CLIs don't STOP their PTY on their
    /// own — once the reply is printed they enter an idle prompt
    /// waiting for next input. Without this, every worker the user
    /// ever spawned stays "alive" in the registry / agent list / DAG
    /// canvas, eating per-agent MCP config files + a phantom PTY.
    ///
    /// We delay the kill by `AUTO_KILL_GRACE_MS` so:
    ///   - the worker can finish printing whatever it's still streaming
    ///   - the asciinema recording's last frames get flushed
    ///   - if the LLM wrote the signal too eagerly mid-thought and
    ///     immediately writes something else (rare), we don't yank it
    ///
    /// Race safety: we re-check the registry on the delayed tick.
    /// The agent may have been manually killed in the meantime, or its
    /// exit_keys entry may have been claimed by `handle_agent_exit`.
    ///
    /// W2-1 verify gate (opt-in): when the worker's role declared
    /// `done_checks`, the kill is held until every check passes in the
    /// worker's cwd — a failed check bounces the delivery back to the
    /// worker instead of accepting "done" (see `verify_gate_before_kill`).
    async fn maybe_auto_kill_on_handoff(&self, path: &str, writer: Option<&str>) {
        // Capture (agent_id, role_label) pairs so the farewell message
        // can sign off with the worker's role instead of an opaque UUID.
        // Only the agent that WROTE its own handoff_signal is reaped (F13):
        // a sibling sharing the same signal string must not be killed when
        // this one finishes. See `select_autokill_targets`. The verify
        // commands ride along from the same registration snapshot.
        let targets: Vec<(String, String, Vec<String>)> = {
            let map = self.exit_keys.read().await;
            select_autokill_targets(&map, path, writer)
                .into_iter()
                .map(|(aid, role)| {
                    let cmds = map
                        .get(&aid)
                        .map(|ek| ek.verify_cmds.clone())
                        .unwrap_or_default();
                    (aid, role, cmds)
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        for (agent_id, role, verify_cmds) in targets {
            let registry = self.registry.clone();
            let swarm = self.swarm.clone();
            let subs = self.subs.clone();
            let exit_keys = self.exit_keys.clone();
            let store = self.store.clone();
            let server_url = self.server_url.clone();
            let sig = path.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(AUTO_KILL_GRACE_MS)).await;
                // W2-1 verify gate: grade what the worker PRODUCED, not what
                // it claims. Fail-closed ordering — the farewell + kill below
                // only run once the gate says pass; a failed check keeps the
                // worker alive and bounces the finding back to it.
                if !verify_cmds.is_empty()
                    && !verify_gate_before_kill(
                        &swarm,
                        &registry,
                        &exit_keys,
                        &server_url,
                        &agent_id,
                        &role,
                        &sig,
                        &verify_cmds,
                    )
                    .await
                {
                    return;
                }
                // Re-check: agent might already be gone.
                let slot = match registry.remove(&agent_id) {
                    Some(s) => s,
                    None => return,
                };
                // FAREWELL MESSAGE: before we tear down the worker, post a
                // short note to the user in the workspace chat. Without
                // this, the worker silently disappears from the member
                // list and users have no idea who to talk to next — they
                // think the project has stopped responding. Magentic-One's
                // PM-style design assumes the orchestrator is "obviously"
                // the one to follow up with, but new users have no such
                // intuition. One sentence in the dying worker's voice
                // fixes the entire confusion class.
                let signal_label = sig.rsplit_once('/').map(|(_, last)| last).unwrap_or(&sig);
                let body = format!(
                    "✓ 已交付 {signal_label} 并解散。继续改 / 加新需求,直接跟 orchestrator 说就行,我俩看同一份 ledger,它清楚我刚才干了啥。",
                );
                let farewell = NewMessage {
                    from_agent: agent_id.clone(),
                    to_agent: "user".into(),
                    kind: "farewell".into(),
                    body,
                    sent_at: now_ms(),
                    in_reply_to: None,
                    // Structured completion → the UI classifies this as a
                    // "completed" notification from meta, not by regex-sniffing
                    // the prose body for ✅/已交付.
                    meta: Some(serde_json::json!({
                        "subtype": "completion",
                        "signal": signal_label,
                    })),
                };
                if let Err(e) = swarm.send_message(farewell).await {
                    tracing::warn!(?e, agent = %agent_id, "auto-kill: farewell send failed");
                }
                // Offload the blocking SIGTERM→grace→SIGKILL: never inline on
                // this worker, never under the slot lock (both stall the whole
                // runtime when a fan-out round auto-kills N workers at once).
                crate::registry::offload_kill(&slot).await;
                swarm.unregister_agent(&agent_id);
                unregister_wake_subs(&subs, &agent_id).await;
                unregister_exit_key(&exit_keys, &agent_id).await;
                if let Err(e) = store.record_agent_kill(agent_id.clone(), now_ms()).await {
                    tracing::warn!(?e, agent = %agent_id, "auto-kill: record_agent_kill failed");
                }
                swarm.publish_event(SwarmEvent::AgentState {
                    agent_id: agent_id.clone(),
                    state: swarmx_protocol::ws_swarm::AgentState::Exited,
                });
                tracing::info!(
                    agent = %agent_id,
                    role = %role,
                    handoff = %sig,
                    "auto-killed worker after handoff_signal"
                );
            });
        }
    }

    /// Producer-died fallback. When an agent transitions to Exited we
    /// look up the `handoff_signal` it was supposed to produce; if that
    /// key isn't on the blackboard yet, write `<signal>.error` so
    /// downstream dependents (test agent waiting on `frontend.done`,
    /// etc.) can detect the upstream failure and branch instead of
    /// hanging forever.
    ///
    /// Best-effort: every failure path is logged and swallowed. We
    /// always clean up the exit_keys entry so a duplicate Exited event
    /// (kill-then-natural-exit race) doesn't try to write again.
    async fn handle_agent_exit(&self, agent_id: &str) {
        let ek = {
            let map = self.exit_keys.read().await;
            match map.get(agent_id) {
                Some(k) if !k.handoff_signal.is_empty() => k.clone(),
                _ => return, // no registered handoff or already cleaned up
            }
        };
        unregister_exit_key(&self.exit_keys, agent_id).await;
        let signal = ek.handoff_signal.clone();

        // Did THIS run's agent write the signal? Query the
        // blackboard_ops history for the path; if the latest write's
        // `at` is newer than our spawn time, we're done — that's our
        // agent's commit. Older `at` means the row is left over from a
        // previous run on the same workspace, and the current agent
        // crashed before producing its own; that's the case we owe an
        // `.error` for.
        let store = self.swarm.store();
        let fresh_by_oplog = match store.list_blackboard_ops(Some(signal.clone())).await {
            Ok(rows) => rows
                .first()
                .map(|r| r.at >= ek.spawned_at_ms)
                .unwrap_or(false),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    agent_id,
                    signal,
                    "list_blackboard_ops failed; falling back to on-disk mtime for freshness"
                );
                false
            }
        };
        // F6: the DISK is the source of truth; the op-log is best-effort. If the
        // op-log says not-fresh, the row may simply have failed to persist while
        // the content DID land on disk — `write_blackboard` keeps the file and
        // broadcasts the wake on an `insert_blackboard_op` failure. Fall back to
        // the on-disk mtime so a worker that actually delivered its handoff isn't
        // handed a spurious `<signal>.error` (which fans out an abort to its
        // dependents). Only stat on the rare not-fresh path.
        let fresh = fresh_by_oplog
            || self
                .swarm
                .blackboard_key_mtime(&signal)
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64 >= ek.spawned_at_ms)
                .unwrap_or(false);
        if fresh {
            tracing::debug!(
                agent_id,
                signal,
                "agent exited with handoff signal already written; no .error needed"
            );
            return;
        }

        // Naming (P0-A): the failure key is the producer's MINTED handoff key
        // + `.error` (e.g. `ws/dir/frontend.done.error`), identical to what a
        // worker is told to write on voluntary failure (see
        // `build_worker_prompt`). One convention for both crash and graceful
        // abort, and `base_key_aliases` fans `<signal>.error` → `<signal>` so
        // even a passive consumer waiting on the success key is woken. `signal`
        // is already the fully-scoped minted key, so no bare `<role>` drift.
        let error_key = format!("{signal}.error");
        let body = serde_json::json!({
            "agent_id": agent_id,
            "role": ek.role,
            "signal": signal,
            "reason": "agent exited without writing its handoff signal",
            "at": now_ms(),
        });
        let body_str = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());
        match self
            .swarm
            .write_blackboard(Some("system".into()), &error_key, &body_str)
            .await
        {
            Ok(_) => {
                tracing::info!(
                    agent_id,
                    signal,
                    error_key,
                    "agent exited without producing signal; wrote .error fallback"
                );
                // NO explicit wake dispatch on this path. The write's own
                // BlackboardChanged broadcast already fans out through
                // `base_key_aliases` (`<signal>.error` → `<signal>`), so the
                // coordinator loop wakes every depends_on=`<signal>` subscriber
                // — plus any literal `.error` subscribers, with writer="system"
                // so nobody is wrongly excluded. Dispatching directly here as
                // well (the pre-dedup code did) delivered TWO identical mailbox
                // rows to every downstream: one from the fan-out, one explicit.
            }
            Err(err) => {
                tracing::warn!(?err, agent_id, error_key, "failed to write .error fallback");
                // The write failed BEFORE anything was broadcast (only an fs
                // failure returns Err — an op-log failure still broadcasts, see
                // F6 in `Swarm::write_blackboard`), so the fan-out above never
                // fires. Wake subscribers of the ORIGINAL signal directly:
                // depends_on lists keys like "frontend.done", not
                // "frontend.done.error" — they at least get a mailbox note
                // describing the upstream failure, even if the .error file is
                // missing.
                let writer = Some(agent_id.to_string());
                let targets = {
                    let map = self.subs.read().await;
                    select_targets(&map, &signal, writer.as_deref())
                };
                for target in targets {
                    self.spawn_deliver_wake(target, error_key.clone());
                }
            }
        }
    }

    /// Is `key` — or its `.error` / `.failed` failure alias — present on the
    /// blackboard right now? Failure aliases mirror the normal dispatch, which
    /// also wakes `depends_on = K` subscribers when `K.error` is written.
    async fn key_or_alias_written(&self, key: &str) -> bool {
        for probe in [
            key.to_string(),
            format!("{key}.error"),
            format!("{key}.failed"),
        ] {
            if matches!(self.swarm.read_blackboard(&probe).await, Ok(Some(_))) {
                return true;
            }
        }
        false
    }

    /// Recover from a broadcast `Lagged` (F12). The coordinator can't know
    /// which events were dropped, so reconcile against ground truth: for every
    /// registered `depends_on` key that's already satisfied on the blackboard,
    /// re-wake the waiting agent. A one-shot handoff wake that was dropped is
    /// otherwise lost forever (no next write, no mailbox row), hanging the
    /// dependent. Re-waking an already-active agent just costs a recheck turn.
    async fn reconcile_after_lag(&self) {
        let map = self.subs.read().await.clone();
        let mut satisfied: std::collections::HashSet<String> = Default::default();
        let mut checked: std::collections::HashSet<String> = Default::default();
        for keys in map.values() {
            for key in keys {
                if !checked.insert(key.clone()) {
                    continue;
                }
                if self.key_or_alias_written(key).await {
                    satisfied.insert(key.clone());
                }
            }
        }
        let rewake = agents_to_rewake(&map, &satisfied);
        if !rewake.is_empty() {
            tracing::warn!(
                count = rewake.len(),
                "wake coordinator: re-waking dependents after broadcast lag \
                 (their awaited key was already on the blackboard)"
            );
        }
        for (agent, key) in rewake {
            self.spawn_deliver_wake(agent, key);
        }
    }

    /// Fan a single `(agent, key)` wake out WITHOUT blocking the coordinator's
    /// single consumer loop. Previously each delivery was awaited inline, so one
    /// wedged opencode/reasonix/zulu endpoint (bound-but-unresponsive, tens of
    /// seconds) stalled EVERY other wake and every producer-death `.error`
    /// fallback swarm-wide. Now the mailbox write + engine kick run in a spawned
    /// task:
    ///   - the mailbox row (source of truth) is always written first;
    ///   - the kick is single-flighted PER AGENT via `kick_locks.try_lock` — a
    ///     concurrent same-agent kick is skipped (its mailbox row is picked up by
    ///     the in-flight kick's turn, the engine's post-turn drain, or the next
    ///     Stop hook), so opencode's unconditional `deliver_turn` can't double-
    ///     submit and two PTY injects can't interleave;
    ///   - `delivery_sem` bounds total in-flight kicks so a burst can't spawn
    ///     unboundedly.
    /// Different agents deliver concurrently — the head-of-line stall is gone,
    /// and a wedged engine holds only its own agent's lock + one permit.
    fn spawn_deliver_wake(&self, target: String, key: String) {
        let swarm = self.swarm.clone();
        let registry = self.registry.clone();
        let subs = self.subs.clone();
        let server_url = self.server_url.clone();
        let sem = self.delivery_sem.clone();
        tokio::spawn(async move {
            // Mailbox is the source of truth: always write it (unless paused or
            // the write fails), independent of the coalesced kick below.
            if !write_wake_mailbox(&swarm, &registry, &target, &key).await {
                return;
            }
            // Per-agent kick single-flight (shared global map, so a manual wake
            // can't race this): try_lock — a same-agent kick already in flight
            // → skip (no permit taken).
            let lock = kick_lock_for(&target);
            let Ok(_kick) = lock.try_lock() else {
                return;
            };
            // Bound actual kicks (not the coalesced ones) so a burst is capped.
            let Ok(_permit) = sem.acquire().await else {
                return; // coordinator shutting down
            };
            kick_agent(&swarm, &registry, &subs, &server_url, &target, &key).await;
        });
    }
}

/// W2-1 verify gate, run after a worker writes its `handoff_signal` and
/// before the auto-kill accepts the completion. Returns `true` when the kill
/// should proceed (every declared check passed), `false` when the worker
/// stays alive — a check failed and the finding was bounced back through the
/// existing wake machinery (mailbox `kind="wake"` row + engine-routed kick).
///
/// The checks run in the worker's own cwd (its registry `workspace`) and are
/// judged by real exit code via `verify::run_verify` (strict allowlist, argv
/// exec, no shell, killpg timebox, output tail). This closes the "the agent
/// lied about running the tests" hole: a handoff write only proves the worker
/// SAYS it finished. First failure decides (hard gate; later checks don't
/// run). The bounce tells the worker to fix and RE-WRITE the same handoff
/// key, which re-triggers this gate; `ExitKey.verify_attempts` caps the loop
/// at `MAX_VERIFY_BOUNCES`, after which the server stops bouncing and tells
/// the worker to escalate — fail-loud, never a silent accept.
///
/// Note: the premature handoff key itself stays on the blackboard (the swarm
/// blackboard has no delete), so dependents may already have been woken by
/// the write; they are woken again when the verified re-write lands. Deleting
/// the unverified key + a persisted `verifying` task status are tracked as
/// phase-3 work in docs/w2-1-verification-gate-design-2026-06-15.md.
#[allow(clippy::too_many_arguments)]
async fn verify_gate_before_kill(
    swarm: &Arc<Swarm>,
    registry: &Registry,
    exit_keys: &ExitKeys,
    server_url: &str,
    agent_id: &str,
    role: &str,
    signal: &str,
    verify_cmds: &[String],
) -> bool {
    // The worker's cwd at spawn time is where its work — and therefore its
    // verification — lives.
    let cwd = match registry.get(agent_id) {
        Some(s) => std::path::PathBuf::from(s.lock().workspace.clone()),
        // Agent already gone (killed / exited during the grace window); the
        // exit path (`handle_agent_exit`) owns the failure semantics now.
        None => return false,
    };
    for cmd in verify_cmds {
        let outcome = crate::verify::run_verify(cmd, &cwd).await;
        if outcome.passed {
            tracing::info!(
                agent = %agent_id,
                role = %role,
                handoff = %signal,
                cmd = %cmd,
                exit_code = ?outcome.exit_code,
                "verify gate: check passed"
            );
            continue;
        }
        // Bounce bookkeeping lives in ExitKey so it dies with the
        // registration (kill/exit) — no cross-run residue.
        let attempts = {
            let mut m = exit_keys.write().await;
            match m.get_mut(agent_id) {
                Some(ek) => {
                    ek.verify_attempts += 1;
                    ek.verify_attempts
                }
                // Unregistered mid-verify (killed/exited) — don't kill here
                // either; the owner of that unregister drives what happens.
                None => return false,
            }
        };
        let gave_up = attempts > MAX_VERIFY_BOUNCES;
        tracing::warn!(
            agent = %agent_id,
            role = %role,
            handoff = %signal,
            cmd = %cmd,
            attempt = attempts,
            gave_up,
            "verify gate FAILED — completion not accepted, bouncing back to worker"
        );
        let body = if gave_up {
            format!(
                "VERIFY GATE: your handoff `{signal}` is still failing its objective check after \
                 {MAX_VERIFY_BOUNCES} fix bounces, so the server stops bouncing here (token-burn \
                 guardrail) — the delivery is still NOT accepted. Latest failure:\n\
                 {}\n\
                 Either fix the underlying problem and re-write `{signal}` via \
                 swarm_write_blackboard, or write `{signal}.error` and report the blocker to the \
                 orchestrator.",
                outcome.detail
            )
        } else {
            format!(
                "VERIFY GATE: your handoff `{signal}` was NOT accepted — an objective check run \
                 in your working directory failed. Do not stop; fix it and deliver again.\n\
                 {}\n\
                 After fixing, re-write `{signal}` via swarm_write_blackboard — the server \
                 re-runs the gate. If the check is genuinely unsatisfiable, write \
                 `{signal}.error` instead so dependents fail loud. (bounce \
                 {attempts}/{MAX_VERIFY_BOUNCES})",
                outcome.detail
            )
        };
        let meta = serde_json::json!({
            "subtype": "wake",
            // NOT "blackboard" → the UI keeps this visible in the feed; a
            // rejected delivery is a real event, not coordination plumbing.
            "reason": "verify",
            "key": signal,
            "attempt": attempts,
        });
        if let Err(e) = deliver_wake_with_body(
            swarm,
            registry,
            server_url,
            agent_id,
            &body,
            meta,
            "verify bounce",
        )
        .await
        {
            // Fail-loud even here: the row failed to persist, so the ONLY
            // record of the bounce is this log line.
            tracing::warn!(?e, agent = %agent_id, "verify gate: bounce delivery failed");
        }
        return false;
    }
    true
}

/// Write the wake mailbox note (source of truth) for `(target, key)`. Returns
/// `false` when the agent is paused (auto-wakes are swallowed, mailbox NOT
/// written) or the write failed — in both cases the caller skips the kick.
async fn write_wake_mailbox(
    swarm: &Arc<Swarm>,
    registry: &Registry,
    target: &str,
    key: &str,
) -> bool {
    // M-pause: if the operator paused this agent, swallow auto-wakes. The
    // mailbox is intentionally NOT written either — paused means "leave me
    // alone, don't accumulate noise I'll have to hand-trim on resume." On resume
    // the operator's deliver_manual_wake writes a single fresh entry.
    if let Some(slot) = registry.get(target) {
        if slot
            .lock()
            .paused
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            tracing::debug!(target, key, "wake skipped: agent is paused");
            return false;
        }
    }
    let now = now_ms();
    let msg = NewMessage {
        from_agent: "system".into(),
        to_agent: target.into(),
        kind: "wake".into(),
        body: wake_mailbox_body(key),
        sent_at: now,
        in_reply_to: None,
        // Auto wake fired by a blackboard change → redundant with the
        // BlackboardChanged event the UI already shows, so the UI filters these
        // out of the feed (it's agent-coordination plumbing). The key stays in
        // the body for the agent that receives it.
        meta: Some(serde_json::json!({
            "subtype": "wake",
            "reason": "blackboard",
            "key": key,
        })),
    };
    if let Err(err) = swarm.send_message(msg).await {
        tracing::warn!(?err, target, key, "wake send_message failed");
        return false;
    }
    true
}

/// Kick `target` to consume its mailbox now (engine-specific). MUST be called
/// serialized per agent (see `spawn_deliver_wake`): opencode's `deliver_turn`
/// is unconditional so two concurrent kicks would double-submit, and two PTY
/// injects would interleave bytes. The mailbox row was already written, so a
/// failed kick is tolerable — the next Stop hook / post-turn drain sees it.
/// Every engine branch that successfully starts a turn also consumes the wake
/// rows it delivered, so the trailing Stop hook noops instead of blocking a
/// second, duplicate turn.
async fn kick_agent(
    swarm: &Arc<Swarm>,
    registry: &Registry,
    subs: &WakeSubs,
    server_url: &str,
    target: &str,
    key: &str,
) {
    let now = now_ms();
    let delivery = match registry.get(target) {
        Some(slot) => crate::input_delivery::LiveDelivery::classify(&slot.lock()),
        None => {
            tracing::debug!(target, key, "wake kick skipped: agent gone");
            // Reap stale subscription so we don't churn on every future write.
            unregister_wake_subs(subs, target).await;
            return;
        }
    };

    // Opencode: pre-consume so swarmx-wake.js sees count=0; kick text is
    // the shared continuation covering all claimed rows (not a count-only
    // recipe). Keystroke: peek without consuming so a failed inject still
    // leaves the mailbox for Stop hook; consume after successful inject.
    let kick_text = match &delivery {
        crate::input_delivery::LiveDelivery::Opencode { .. } => {
            match crate::wake_claim::claim(swarm, target, now).await {
                Ok(resp) if resp.count > 0 => resp.continuation(),
                Ok(_) => wake_mailbox_body(key),
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        target,
                        key,
                        "opencode pre-consume wakes failed; plugin may double-kick"
                    );
                    wake_mailbox_body(key)
                }
            }
        }
        crate::input_delivery::LiveDelivery::Keystroke => {
            match crate::wake_claim::peek(swarm, target).await {
                Ok(resp) if resp.count > 0 => resp.continuation(),
                _ => wake_mailbox_body(key),
            }
        }
        _ => wake_mailbox_body(key),
    };

    match crate::input_delivery::deliver_wake_turn(
        registry,
        target,
        &kick_text,
        key,
        crate::input_delivery::WakeTurnCtx {
            server_url,
            reasonix_body: None,
        },
    )
    .await
    {
        Ok(crate::input_delivery::WakeChannel::Keystroke) => {
            // Consume after PTY inject so Stop hook noops (double-token fix).
            if let Err(err) = crate::wake_claim::claim(swarm, target, now).await {
                tracing::warn!(
                    ?err,
                    target,
                    key,
                    "post-inject consume_wakes failed; Stop hook will re-deliver"
                );
            }
            tracing::info!(target, key, "wake delivered");
        }
        Ok(ch) => {
            tracing::info!(target, key, channel = ?ch, "wake delivered via TurnDelivery");
        }
        Err(err) => {
            tracing::warn!(?err, target, key, "wake kick failed");
            // PTY-missing / agent-gone: drop stale subscription so we don't churn.
            if matches!(delivery, crate::input_delivery::LiveDelivery::Keystroke) {
                unregister_wake_subs(subs, target).await;
            }
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Detect cycles in a {role → depends_on} graph using BFS/DFS. Returns
/// `Ok(())` if acyclic, `Err` listing the cycle path otherwise. Called
/// from `run_spell` before any agent is spawned so we fail fast on bad
/// manifests rather than producing 3 agents that deadlock waiting on
/// each other.
///
/// Note: `depends_on` values are blackboard *keys* (e.g. `frontend.done`)
/// not role ids. To detect cycles we map each key back to the role that
/// declares its `handoff_signal` equal to that key. Keys with no
/// producing role are treated as external inputs (no cycle through them).
pub fn detect_depends_on_cycles(
    role_handoff: &HashMap<String, String>, // role_name → handoff_signal (the key it produces)
    role_depends: &HashMap<String, Vec<String>>, // role_name → depends_on keys
) -> Result<()> {
    // Reverse-lookup: which role produces this key?
    let key_to_role: HashMap<&str, &str> = role_handoff
        .iter()
        .filter(|(_, k)| !k.is_empty())
        .map(|(r, k)| (k.as_str(), r.as_str()))
        .collect();

    // For each role, do a DFS through its depended-on keys' producers.
    // If we ever revisit the starting role, we have a cycle.
    let role_names: Vec<&str> = role_depends.keys().map(String::as_str).collect();
    for start in &role_names {
        let mut stack: Vec<&str> = vec![*start];
        let mut visiting: std::collections::HashSet<&str> = std::collections::HashSet::new();
        while let Some(current) = stack.pop() {
            if !visiting.insert(current) {
                continue;
            }
            let deps = match role_depends.get(current) {
                Some(d) => d,
                None => continue,
            };
            for dep_key in deps {
                if let Some(producer) = key_to_role.get(dep_key.as_str()) {
                    if *producer == *start {
                        return Err(anyhow!(
                            "depends_on cycle: role `{start}` (via key `{dep_key}`) depends back on itself"
                        ));
                    }
                    stack.push(*producer);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_subs(entries: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(aid, keys)| {
                (
                    aid.to_string(),
                    keys.iter().map(|k| k.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn kick_lock_try_lock_single_flights_same_agent() {
        // Mirrors spawn_deliver_wake: a second same-agent kick must not block
        // the coordinator — try_lock fails while the first holds the mutex.
        let lock = kick_lock_for("agent-a");
        let held = lock.try_lock().expect("first kick acquires");
        assert!(
            lock.try_lock().is_err(),
            "concurrent same-agent kick must be skipped (single-flight)"
        );
        drop(held);
        assert!(
            lock.try_lock().is_ok(),
            "after release, next kick may proceed"
        );
    }

    #[test]
    fn delivery_sem_caps_concurrent_kicks() {
        // Documents the A′ mitigation: wedged engines consume permits but
        // cannot unbounded-spawn kick tasks.
        assert!(
            MAX_CONCURRENT_KICKS >= 4,
            "MAX_CONCURRENT_KICKS must stay high enough for a small swarm fan-out"
        );
        let sem = Semaphore::new(MAX_CONCURRENT_KICKS);
        assert_eq!(sem.available_permits(), MAX_CONCURRENT_KICKS);
    }

    #[test]
    fn select_targets_empty_map_returns_empty() {
        let m: HashMap<String, Vec<String>> = HashMap::new();
        assert!(select_targets(&m, "any.key", None).is_empty());
        assert!(select_targets(&m, "any.key", Some("nobody")).is_empty());
    }

    #[test]
    fn select_targets_picks_only_subscribers_of_key() {
        let m = build_subs(&[
            ("test-a", &["frontend.done", "backend.done"]),
            ("fe-a", &[]),
            ("be-a", &[]),
        ]);
        let mut t = select_targets(&m, "backend.done", None);
        t.sort();
        assert_eq!(t, vec!["test-a".to_string()]);
    }

    #[test]
    fn select_targets_excludes_writer() {
        let m = build_subs(&[("test-a", &["x.done"]), ("self-watcher", &["x.done"])]);
        let t = select_targets(&m, "x.done", Some("self-watcher"));
        assert_eq!(t, vec!["test-a".to_string()]);
    }

    #[test]
    fn select_targets_external_edit_wakes_all_subscribers() {
        // writer = None means an external (filesystem) edit; everyone
        // subscribed to the key should be woken.
        let m = build_subs(&[("a", &["k"]), ("b", &["k"]), ("c", &["other"])]);
        let mut t = select_targets(&m, "k", None);
        t.sort();
        assert_eq!(t, vec!["a".to_string(), "b".to_string()]);
    }

    fn build_exit_keys(entries: &[(&str, &str, &str)]) -> HashMap<String, ExitKey> {
        entries
            .iter()
            .map(|(aid, role, sig)| {
                (
                    aid.to_string(),
                    ExitKey {
                        role: role.to_string(),
                        handoff_signal: sig.to_string(),
                        spawned_at_ms: 0,
                        verify_cmds: Vec::new(),
                        verify_attempts: 0,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn autokill_reaps_only_the_writer_not_siblings_sharing_signal() {
        // F13: worker-a and worker-b BOTH declare handoff "demo/out.done".
        // worker-a writes it → only worker-a is reaped; worker-b (possibly
        // still working) is left alone.
        let ek = build_exit_keys(&[
            ("worker-a", "writer", "demo/out.done"),
            ("worker-b", "writer", "demo/out.done"),
        ]);
        let t = select_autokill_targets(&ek, "demo/out.done", Some("worker-a"));
        assert_eq!(t, vec![("worker-a".to_string(), "writer".to_string())]);
    }

    #[test]
    fn autokill_unattributed_write_reaps_nobody() {
        // writer = None (external editor / reconcile) — never auto-kill.
        let ek = build_exit_keys(&[("worker-a", "writer", "demo/out.done")]);
        assert!(select_autokill_targets(&ek, "demo/out.done", None).is_empty());
    }

    #[test]
    fn autokill_writer_with_unrelated_path_reaps_nobody() {
        // worker-a wrote some OTHER key, not its own handoff_signal → not done.
        let ek = build_exit_keys(&[("worker-a", "writer", "demo/out.done")]);
        assert!(select_autokill_targets(&ek, "demo/progress.md", Some("worker-a")).is_empty());
    }

    #[test]
    fn autokill_empty_map_returns_empty() {
        let ek: HashMap<String, ExitKey> = HashMap::new();
        assert!(select_autokill_targets(&ek, "x", Some("a")).is_empty());
    }

    #[test]
    fn orphaned_handoff_warns_on_depends_on_mismatch() {
        // Producer's handoff is "ws-42/api.done"; the dependent drifted to
        // "api.done" (dropped the workspace prefix) → nobody matches → orphan.
        // Returns the keys agents ARE waiting on, for the warning context.
        let subs = build_subs(&[("be", &["api.done"])]);
        let handoffs = vec!["ws-42/api.done".to_string()];
        let got = orphaned_handoff_diagnosis(&subs, &handoffs, "ws-42/api.done", false);
        assert_eq!(got, Some(vec!["api.done".to_string()]));
    }

    #[test]
    fn orphaned_handoff_silent_when_a_subscriber_matched() {
        // woke_anyone = true (the fan-out delivered) → never warn.
        let subs = build_subs(&[("be", &["ws/api.done"])]);
        let handoffs = vec!["ws/api.done".to_string()];
        assert_eq!(
            orphaned_handoff_diagnosis(&subs, &handoffs, "ws/api.done", true),
            None
        );
    }

    #[test]
    fn orphaned_handoff_silent_for_non_handoff_writes() {
        // A routine scratch/ledger write that isn't any agent's handoff_signal
        // must NOT warn, even with zero subscribers — keeps the signal noise-free.
        let subs = build_subs(&[("be", &["ws/api.done"])]);
        let handoffs = vec!["ws/api.done".to_string()];
        assert_eq!(
            orphaned_handoff_diagnosis(&subs, &handoffs, "ws/progress.ledger.md", false),
            None
        );
    }

    #[test]
    fn agents_to_rewake_picks_only_satisfied_dependents() {
        // be + qa depend on a satisfied key → re-wake; fe's key is not
        // satisfied → skip. Output is deterministically sorted.
        let subs = build_subs(&[
            ("be", &["ws/api.done"]),
            ("fe", &["ws/ui.done"]),
            ("qa", &["ws/api.done", "ws/ui.done"]),
        ]);
        let mut satisfied = std::collections::HashSet::new();
        satisfied.insert("ws/api.done".to_string());
        let got = agents_to_rewake(&subs, &satisfied);
        assert_eq!(
            got,
            vec![
                ("be".to_string(), "ws/api.done".to_string()),
                ("qa".to_string(), "ws/api.done".to_string()),
            ]
        );
    }

    #[test]
    fn agents_to_rewake_empty_when_nothing_satisfied() {
        let subs = build_subs(&[("be", &["ws/api.done"])]);
        let satisfied = std::collections::HashSet::new();
        assert!(agents_to_rewake(&subs, &satisfied).is_empty());
    }

    #[test]
    fn select_targets_no_match_returns_empty() {
        let m = build_subs(&[("a", &["foo.done"])]);
        assert!(select_targets(&m, "bar.done", None).is_empty());
    }

    // ── P0-A: minted keys match exactly; drift no longer silently no-wakes ──

    #[test]
    fn minted_key_matches_exactly_drift_does_not() {
        // Consumer subscribes to the canonical minted key.
        let minted = "ws_ab12/dark-mode/frontend.done";
        let m = build_subs(&[("consumer", &[minted])]);
        // The producer's minted write wakes it.
        assert_eq!(
            select_targets(&m, minted, Some("frontend")),
            vec!["consumer"]
        );
        // A drifted key (missing the workspace/thread prefix — the exact F3
        // failure) matches NOTHING. Under the old free-string scheme this is
        // how a dependent hung forever; under P0-A both sides are server-minted
        // so this drift can't be produced, and if it somehow were, it's inert.
        assert!(select_targets(&m, "frontend.done", None).is_empty());
    }

    #[test]
    fn minted_error_key_fans_out_to_the_success_key() {
        // A worker (or the death fallback) writing `<minted>.error` must wake
        // the consumers that wait on `<minted>` (the success key), via the
        // base-key alias fan-out — that's the fail-LOUD path.
        let minted = "ws_ab12/dark-mode/frontend.done";
        let error_key = format!("{minted}.error");
        assert_eq!(base_key_aliases(&error_key), vec![minted.to_string()]);

        let m = build_subs(&[("consumer", &[minted])]);
        // Simulate the BlackboardChanged fan: literal key + its base aliases.
        let mut woke: Vec<String> = Vec::new();
        let mut keys = vec![error_key.clone()];
        keys.extend(base_key_aliases(&error_key));
        for k in &keys {
            woke.extend(select_targets(&m, k, Some("frontend")));
        }
        assert_eq!(
            woke,
            vec!["consumer"],
            "the .done waiter is woken on .error"
        );
    }

    #[tokio::test]
    async fn register_and_unregister_round_trip() {
        let subs: WakeSubs = Arc::new(RwLock::new(HashMap::new()));
        register_wake_subs(&subs, "a".into(), vec!["k1".into(), "k2".into()]).await;
        assert_eq!(subs.read().await.get("a").map(|v| v.len()), Some(2));
        unregister_wake_subs(&subs, "a").await;
        assert!(subs.read().await.get("a").is_none());
    }

    #[tokio::test]
    async fn register_ignores_empty_keys() {
        let subs: WakeSubs = Arc::new(RwLock::new(HashMap::new()));
        register_wake_subs(&subs, "a".into(), vec![]).await;
        assert!(
            subs.read().await.get("a").is_none(),
            "empty depends_on shouldn't pollute the map"
        );
    }

    #[tokio::test]
    async fn inject_wake_kick_errors_on_missing_agent() {
        let registry = Registry::new();
        let err = inject_wake_kick(&registry, "http://127.0.0.1:7777", "ghost", "k")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("ghost"));
    }

    // ── PTY kick consume: one wake = ONE turn, no duplicate Stop-hook turn ──

    /// Live PTY-backed slot for kick tests — same construction as
    /// `reaper.rs`'s `agent_slot_for`, with one twist: the child is
    /// `sh -c 'read x; exit 0'`, so the wake kick's trailing `\r` completes
    /// the `read` and the child EXITS ON ITS OWN right after the inject.
    ///
    /// The returned `output_rx` MUST stay alive until the slot's bridge is
    /// dropped (the caller drops the registry first, the receiver last):
    /// dropping it early kills the reader thread on its first
    /// `blocking_send`, the pty master's output side is then never drained,
    /// and the tty echo of the injected bytes is still pending when the
    /// child exits — on macOS that wedges the child mid-exit (STAT E) and
    /// `PtyBridge::kill`'s `wait4` never returns. Observed as this test
    /// hanging past the 60s harness warning with `cat`, `sleep 30`, and
    /// self-exiting children alike; reaper.rs's `exit 0`/`sleep 10` slots
    /// never hit it only because they never write to the pty input (no
    /// input → no echo → nothing pending at exit).
    #[cfg(unix)]
    fn live_pty_slot() -> (
        crate::registry::AgentSlot,
        tokio::sync::mpsc::Receiver<Bytes>,
    ) {
        use crate::pty_stream::PtyStream;
        use crate::registry::{AgentChannel, AgentSlot, Lifecycle};
        use std::sync::atomic::AtomicBool;
        use swarmx_pty::{PtyBridge, PtyHandles, SpawnOpts};

        let PtyHandles { bridge, output_rx } = PtyBridge::spawn(SpawnOpts {
            argv: &["/bin/sh".into(), "-c".into(), "read x; exit 0".into()],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn test child");
        let input_tx = bridge.input_sender();
        let (lifecycle_tx, _rx) = tokio::sync::broadcast::channel(16);
        let slot = AgentSlot {
            channel: AgentChannel::Pty {
                bridge: Arc::new(bridge),
                stream: Arc::new(PtyStream::new()),
                input_tx,
            },
            lifecycle: Arc::new(parking_lot::Mutex::new(Lifecycle::default())),
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
        };
        (slot, output_rx)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn pty_kick_consumes_wake_rows_so_stop_hook_sees_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            swarmx_storage::Store::open(&dir.path().join("db.sqlite"))
                .await
                .unwrap(),
        );
        let swarm = Swarm::new(store.clone(), dir.path().join("bb"));
        let registry = Registry::new();
        let (slot, output_rx) = live_pty_slot();
        registry.insert("worker".into(), slot);
        let subs: WakeSubs = Arc::new(RwLock::new(HashMap::new()));

        // Same order as spawn_deliver_wake: mailbox row first, then the kick.
        assert!(write_wake_mailbox(&swarm, &registry, "worker", "ws/a.done").await);
        assert_eq!(store.count_unread("worker".into()).await.unwrap(), 1);

        kick_agent(
            &swarm,
            &registry,
            &subs,
            "http://127.0.0.1:1",
            "worker",
            "ws/a.done",
        )
        .await;

        // What wake-check (the Stop hook) would claim at the end of the
        // injected turn: nothing — so it noops instead of blocking a second,
        // content-duplicate turn.
        let left = store
            .consume_wakes("worker".into(), now_ms())
            .await
            .unwrap();
        assert!(
            left.is_empty(),
            "delivered wake rows must be consumed by the kick"
        );
        assert_eq!(store.count_unread("worker".into()).await.unwrap(), 0);

        // Teardown ORDER matters (see live_pty_slot): kill the bridge while
        // the reader thread can still drain the pty, drop the receiver last.
        drop(registry);
        drop(output_rx);
    }

    #[tokio::test]
    async fn failed_pty_kick_keeps_wake_rows_for_stop_hook_fallback() {
        // The flip side: an inject FAILURE must leave the mailbox row unread
        // so the next Stop hook still force-wakes the agent (belt-and-
        // suspenders), and the dead agent's subscription gets reaped.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            swarmx_storage::Store::open(&dir.path().join("db.sqlite"))
                .await
                .unwrap(),
        );
        let swarm = Swarm::new(store.clone(), dir.path().join("bb"));
        let registry = Registry::new(); // no slot for "ghost" → inject errors
        let subs: WakeSubs = Arc::new(RwLock::new(HashMap::new()));
        register_wake_subs(&subs, "ghost".into(), vec!["ws/a.done".into()]).await;

        assert!(write_wake_mailbox(&swarm, &registry, "ghost", "ws/a.done").await);
        kick_agent(
            &swarm,
            &registry,
            &subs,
            "http://127.0.0.1:1",
            "ghost",
            "ws/a.done",
        )
        .await;

        let left = store.consume_wakes("ghost".into(), now_ms()).await.unwrap();
        assert_eq!(
            left.len(),
            1,
            "failed kick must not consume the wake fallback"
        );
        assert!(
            subs.read().await.get("ghost").is_none(),
            "dead agent's subscription reaped"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn manual_wake_pty_path_consumes_wake_row() {
        // Regression: the manual ⚡ wake on claude/codex/kimi injected the PTY
        // turn but never consumed the mailbox wake row, so the trailing Stop
        // hook (wake-check) claimed it and forced a SECOND, content-duplicate
        // turn — every manual wake cost double tokens.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            swarmx_storage::Store::open(&dir.path().join("db.sqlite"))
                .await
                .unwrap(),
        );
        let swarm = Swarm::new(store.clone(), dir.path().join("bb"));
        let registry = Registry::new();
        let (slot, output_rx) = live_pty_slot();
        registry.insert("manual-worker".into(), slot);

        deliver_manual_wake(&swarm, &registry, "http://127.0.0.1:1", "manual-worker")
            .await
            .unwrap();

        let msgs = store
            .list_messages(swarmx_storage::ListMessagesOpts {
                to_agent: Some("manual-worker".into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1, "exactly one wake row written");
        assert!(msgs[0].body.contains("操作员唤醒"));
        assert_eq!(
            store.count_unread("manual-worker".into()).await.unwrap(),
            0,
            "the delivered wake row must be consumed so the Stop hook noops"
        );
        // Teardown ORDER matters (see live_pty_slot).
        drop(registry);
        drop(output_rx);
    }

    // ── W2-1 verify gate: a handoff write ≠ done until the checks pass ────

    /// Insert a live PTY-backed worker whose cwd is `ws_dir` (created), so
    /// verify commands run somewhere hermetic. Returns the receiver that MUST
    /// outlive the registry drop (see live_pty_slot).
    #[cfg(unix)]
    fn insert_pty_worker(
        registry: &Registry,
        agent: &str,
        ws_dir: &std::path::Path,
    ) -> tokio::sync::mpsc::Receiver<Bytes> {
        std::fs::create_dir_all(ws_dir).unwrap();
        let (slot, output_rx) = live_pty_slot();
        let slot = crate::registry::AgentSlot {
            workspace: ws_dir.to_string_lossy().into_owned(),
            ..slot
        };
        registry.insert(agent.into(), slot);
        output_rx
    }

    #[cfg(unix)]
    async fn gate_fixtures() -> (
        tempfile::TempDir,
        Arc<swarmx_storage::Store>,
        Arc<Swarm>,
        Registry,
        ExitKeys,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            swarmx_storage::Store::open(&dir.path().join("db.sqlite"))
                .await
                .unwrap(),
        );
        let swarm = Swarm::new(store.clone(), dir.path().join("bb"));
        let registry = Registry::new();
        let exit_keys: ExitKeys = Arc::new(RwLock::new(HashMap::new()));
        (dir, store, swarm, registry, exit_keys)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn verify_gate_pass_allows_completion() {
        let (dir, store, swarm, registry, exit_keys) = gate_fixtures().await;
        let output_rx = insert_pty_worker(&registry, "pass-worker", &dir.path().join("ws"));
        register_exit_key(
            &exit_keys,
            "pass-worker".into(),
            "backend".into(),
            "ws/a.done".into(),
            now_ms(),
            vec!["node --version".to_string()],
        )
        .await;

        let proceed = verify_gate_before_kill(
            &swarm,
            &registry,
            &exit_keys,
            "http://127.0.0.1:1",
            "pass-worker",
            "backend",
            "ws/a.done",
            &["node --version".to_string()],
        )
        .await;
        assert!(proceed, "passing checks let the completion through");
        assert_eq!(
            exit_keys.read().await["pass-worker"].verify_attempts,
            0,
            "a pass is not a bounce"
        );
        assert_eq!(
            store.count_unread("pass-worker".into()).await.unwrap(),
            0,
            "no bounce mail on pass"
        );
        drop(registry);
        drop(output_rx);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn verify_gate_failure_bounces_back_and_worker_stays_alive() {
        let (dir, store, swarm, registry, exit_keys) = gate_fixtures().await;
        // `cargo test` in an empty dir fails fast (no Cargo.toml, exit 101).
        // (Unique agent name per test: kick_locks() is process-global, and a
        // same-named agent in a concurrently-running test would coalesce this
        // test's kick, leaving its wake row unconsumed.)
        let output_rx = insert_pty_worker(&registry, "vfy-worker", &dir.path().join("ws"));
        register_exit_key(
            &exit_keys,
            "vfy-worker".into(),
            "backend".into(),
            "ws/a.done".into(),
            now_ms(),
            vec!["cargo test".to_string()],
        )
        .await;

        let proceed = verify_gate_before_kill(
            &swarm,
            &registry,
            &exit_keys,
            "http://127.0.0.1:1",
            "vfy-worker",
            "backend",
            "ws/a.done",
            &["cargo test".to_string()],
        )
        .await;
        assert!(!proceed, "a failed check must block the completion");
        assert!(
            registry.get("vfy-worker").is_some(),
            "worker stays alive to fix and re-deliver"
        );
        assert_eq!(exit_keys.read().await["vfy-worker"].verify_attempts, 1);
        let msgs = store
            .list_messages(swarmx_storage::ListMessagesOpts {
                to_agent: Some("vfy-worker".into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1, "one bounce message");
        assert!(
            msgs[0].body.contains("VERIFY GATE"),
            "got: {}",
            msgs[0].body
        );
        assert!(
            msgs[0].body.contains("re-write") && msgs[0].body.contains("ws/a.done"),
            "the bounce names the key to re-deliver: {}",
            msgs[0].body
        );
        assert!(
            msgs[0].body.contains("FAILED"),
            "the bounce carries the check evidence: {}",
            msgs[0].body
        );
        // The PTY delivery consumed the wake row (one wake = ONE turn).
        assert_eq!(store.count_unread("vfy-worker".into()).await.unwrap(), 0);
        drop(registry);
        drop(output_rx);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn verify_gate_stops_bouncing_after_cap() {
        let (dir, store, swarm, registry, exit_keys) = gate_fixtures().await;
        let output_rx = insert_pty_worker(&registry, "cap-worker", &dir.path().join("ws"));
        register_exit_key(
            &exit_keys,
            "cap-worker".into(),
            "backend".into(),
            "ws/a.done".into(),
            now_ms(),
            vec!["cargo test".to_string()],
        )
        .await;
        // Pre-load the counter at the cap: the NEXT failure must be the
        // give-up escalation, not another fix bounce (token-burn guardrail).
        exit_keys
            .write()
            .await
            .get_mut("cap-worker")
            .unwrap()
            .verify_attempts = MAX_VERIFY_BOUNCES;

        let proceed = verify_gate_before_kill(
            &swarm,
            &registry,
            &exit_keys,
            "http://127.0.0.1:1",
            "cap-worker",
            "backend",
            "ws/a.done",
            &["cargo test".to_string()],
        )
        .await;
        assert!(!proceed, "still not accepted");
        let msgs = store
            .list_messages(swarmx_storage::ListMessagesOpts {
                to_agent: Some("cap-worker".into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].body.contains("stops bouncing"),
            "give-up branch, got: {}",
            msgs[0].body
        );
        assert!(
            msgs[0].body.contains("ws/a.done.error"),
            "escalation path named: {}",
            msgs[0].body
        );
        drop(registry);
        drop(output_rx);
    }

    /// Full wiring through `maybe_auto_kill_on_handoff` (with the real 5s
    /// auto-kill grace): no `done_checks` → legacy kill; passing checks →
    /// gate then kill; failing check → bounce, no kill.
    #[tokio::test]
    #[cfg(unix)]
    async fn auto_kill_respects_verify_gate_pass_and_fail() {
        let (dir, store, swarm, registry, exit_keys) = gate_fixtures().await;
        let subs: WakeSubs = Arc::new(RwLock::new(HashMap::new()));
        let mut receivers = Vec::new();
        for (agent, cmds) in [
            ("w-plain", Vec::new()),
            ("w-pass", vec!["node --version".to_string()]),
            ("w-fail", vec!["cargo test".to_string()]),
        ] {
            receivers.push(insert_pty_worker(&registry, agent, &dir.path().join("ws")));
            register_exit_key(
                &exit_keys,
                agent.into(),
                "backend".into(),
                format!("ws/{agent}.done"),
                now_ms(),
                cmds,
            )
            .await;
        }
        let coord = WakeCoordinator {
            swarm: swarm.clone(),
            registry: registry.clone(),
            subs: subs.clone(),
            exit_keys: exit_keys.clone(),
            store: store.clone(),
            server_url: "http://127.0.0.1:1".into(),
            delivery_sem: Arc::new(Semaphore::new(4)),
        };
        for agent in ["w-plain", "w-pass", "w-fail"] {
            coord
                .maybe_auto_kill_on_handoff(&format!("ws/{agent}.done"), Some(agent))
                .await;
        }

        // The kill fires after AUTO_KILL_GRACE_MS plus gate runtime; poll.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let plain_gone = registry.get("w-plain").is_none();
            let pass_gone = registry.get("w-pass").is_none();
            let bounced = store
                .list_messages(swarmx_storage::ListMessagesOpts {
                    to_agent: Some("w-fail".into()),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .unwrap()
                .iter()
                .any(|m| m.body.contains("VERIFY GATE"));
            if plain_gone && pass_gone && bounced {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting: plain_gone={plain_gone} pass_gone={pass_gone} bounced={bounced}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // Gate disabled (plain) and gate passed (pass) → both reaped; gate
        // failed (fail) → still registered, and stays that way (no late kill).
        assert!(registry.get("w-plain").is_none());
        assert!(registry.get("w-pass").is_none());
        assert!(registry.get("w-fail").is_some());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            registry.get("w-fail").is_some(),
            "a failed verify gate must not kill the worker"
        );
        assert_eq!(
            exit_keys.read().await["w-fail"].verify_attempts,
            1,
            "one bounce recorded"
        );
        // Teardown ORDER matters (see live_pty_slot).
        drop(registry);
        drop(receivers);
    }

    #[tokio::test]
    async fn producer_death_wakes_dependent_exactly_once() {
        // Regression for the double-channel wake: `handle_agent_exit` writes
        // `<signal>.error`, whose BlackboardChanged broadcast fans out to the
        // base key's subscribers via `base_key_aliases`. The old explicit
        // dispatch on top of that delivered a SECOND identical mailbox row.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            swarmx_storage::Store::open(&dir.path().join("db.sqlite"))
                .await
                .unwrap(),
        );
        // The blackboard root must exist for the .error write to succeed —
        // this test only guards the dedup when the write's own broadcast
        // fires (the Ok path; the Err path is a single dispatch either way).
        std::fs::create_dir_all(dir.path().join("bb")).unwrap();
        let swarm = Swarm::new(store.clone(), dir.path().join("bb"));
        let registry = Registry::new();
        let subs: WakeSubs = Arc::new(RwLock::new(HashMap::new()));
        let exit_keys: ExitKeys = Arc::new(RwLock::new(HashMap::new()));
        register_wake_subs(&subs, "dep".into(), vec!["ws/sig.done".into()]).await;
        // spawned_at in the future → the freshness check fails → .error written.
        register_exit_key(
            &exit_keys,
            "prod".into(),
            "role".into(),
            "ws/sig.done".into(),
            now_ms() + 60_000,
            Vec::new(),
        )
        .await;
        let _coordinator = WakeCoordinator::spawn(
            swarm.clone(),
            registry.clone(),
            subs.clone(),
            exit_keys.clone(),
            store.clone(),
            "http://127.0.0.1:1".into(),
        );

        // The coordinator task subscribes to the broadcast only when it is
        // first polled. `publish_event` below is synchronous, so without this
        // yield the AgentState event is emitted BEFORE the subscription
        // exists and is silently missed (broadcast receivers only see what
        // lands after they subscribe) — the test then fails on rows==0.
        tokio::task::yield_now().await;
        swarm.publish_event(SwarmEvent::AgentState {
            agent_id: "prod".into(),
            state: swarmx_protocol::ws_swarm::AgentState::Exited,
        });

        // Wait for the .error fallback + fan-out to land in dep's mailbox,
        // then settle to give any duplicate dispatch time to appear too.
        let mut rows = 0;
        for _ in 0..250 {
            rows = store.count_unread("dep".into()).await.unwrap();
            if rows > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(rows, 1, "dependent must get the .error wake");
        assert!(
            dir.path().join("bb/ws/sig.done.error").exists(),
            "the .error fallback landed on disk (Ok path, fan-out delivered the wake)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            store.count_unread("dep".into()).await.unwrap(),
            1,
            "no duplicate wake row from a second dispatch channel"
        );
    }

    // ── M6c step 5: exit_keys + .error/.failed fan-out ──────────────────

    #[tokio::test]
    async fn exit_key_register_and_unregister() {
        let keys: ExitKeys = Arc::new(RwLock::new(HashMap::new()));
        register_exit_key(
            &keys,
            "a".into(),
            "frontend".into(),
            "frontend.done".into(),
            1_700_000_000_000,
            Vec::new(),
        )
        .await;
        let stored = keys.read().await.get("a").cloned();
        assert_eq!(stored.as_ref().map(|k| k.role.as_str()), Some("frontend"));
        assert_eq!(
            stored.as_ref().map(|k| k.handoff_signal.as_str()),
            Some("frontend.done")
        );
        assert_eq!(stored.map(|k| k.spawned_at_ms), Some(1_700_000_000_000));
        unregister_exit_key(&keys, "a").await;
        assert!(keys.read().await.get("a").is_none());
    }

    #[tokio::test]
    async fn exit_key_register_ignores_empty_signal() {
        // planner has no handoff_signal; we shouldn't pollute the map.
        let keys: ExitKeys = Arc::new(RwLock::new(HashMap::new()));
        register_exit_key(
            &keys,
            "planner-a".into(),
            "planner".into(),
            "".into(),
            1_700_000_000_000,
            Vec::new(),
        )
        .await;
        assert!(
            keys.read().await.get("planner-a").is_none(),
            "empty handoff_signal shouldn't pollute exit_keys"
        );
    }

    #[test]
    fn base_key_aliases_strips_error_suffix() {
        assert_eq!(
            base_key_aliases("frontend.done.error"),
            vec!["frontend.done"]
        );
    }

    #[test]
    fn base_key_aliases_strips_failed_suffix() {
        assert_eq!(
            base_key_aliases("backend.done.failed"),
            vec!["backend.done"]
        );
    }

    #[test]
    fn base_key_aliases_passes_through_plain_key() {
        // Regular key (no suffix) → no fan-out, the wake loop wakes only
        // the literal-key subscribers as before.
        assert!(base_key_aliases("frontend.done").is_empty());
        assert!(base_key_aliases("api.spec").is_empty());
    }

    #[test]
    fn base_key_aliases_handles_bare_suffix() {
        // ".error" with empty base — definitely not a real handoff key
        // anyone subscribed to. Empty Vec → no fan-out.
        assert!(base_key_aliases(".error").is_empty());
        assert!(base_key_aliases(".failed").is_empty());
    }

    #[test]
    fn wake_mailbox_body_success_vs_failure() {
        assert_eq!(
            wake_mailbox_body("ws/t/researcher.done"),
            "共享区 `ws/t/researcher.done` 有更新，请查看"
        );
        let fail = wake_mailbox_body("ws/t/researcher.a4f1fcf3.done.error");
        assert!(fail.contains("失败"), "{fail}");
        assert!(
            fail.contains("ws/t/researcher.a4f1fcf3.done.error"),
            "{fail}"
        );
        assert!(fail.contains("instance key"), "{fail}");
        assert!(!fail.contains("有更新"), "{fail}");
        let failed = wake_mailbox_body("ws/t/backend.done.failed");
        assert!(failed.contains("失败"), "{failed}");
        assert!(failed.contains("ws/t/backend.done"), "{failed}");
    }

    #[test]
    fn fanout_wakes_base_key_subscribers_on_error() {
        // dependent subscribes to "frontend.done"; .error write should
        // reach them via base_key_aliases → select_targets("frontend.done").
        let map = build_subs(&[("test-a", &["frontend.done"])]);
        // Direct hit on the .error key — no subscribers.
        assert!(select_targets(&map, "frontend.done.error", None).is_empty());
        // But the aliased base key picks up the dependent.
        let aliases = base_key_aliases("frontend.done.error");
        assert_eq!(aliases, vec!["frontend.done"]);
        let woken = select_targets(&map, &aliases[0], None);
        assert_eq!(woken, vec!["test-a".to_string()]);
    }

    // ── cycle detection ─────────────────────────────────────────────────

    fn map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn mapv(entries: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn cycle_detection_passes_acyclic_fullstack_layout() {
        // The actual M6a topology: test depends on frontend+backend; nobody
        // depends on test.
        let handoff = map(&[
            ("frontend", "frontend.done"),
            ("backend", "backend.done"),
            ("test", "test.passed"),
        ]);
        let deps = mapv(&[
            ("frontend", &[]),
            ("backend", &[]),
            ("test", &["frontend.done", "backend.done"]),
        ]);
        assert!(detect_depends_on_cycles(&handoff, &deps).is_ok());
    }

    #[test]
    fn cycle_detection_catches_self_loop() {
        let handoff = map(&[("a", "a.done")]);
        let deps = mapv(&[("a", &["a.done"])]);
        let err = detect_depends_on_cycles(&handoff, &deps).unwrap_err();
        assert!(format!("{err:#}").contains("cycle"));
    }

    #[test]
    fn cycle_detection_catches_two_role_cycle() {
        let handoff = map(&[("a", "a.done"), ("b", "b.done")]);
        let deps = mapv(&[("a", &["b.done"]), ("b", &["a.done"])]);
        let err = detect_depends_on_cycles(&handoff, &deps).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("cycle"));
    }

    #[test]
    fn cycle_detection_ignores_unrooted_keys() {
        // depends_on points at a key nobody produces — treated as
        // external input, no cycle.
        let handoff = map(&[("a", "a.done")]);
        let deps = mapv(&[("a", &["external.signal"])]);
        assert!(detect_depends_on_cycles(&handoff, &deps).is_ok());
    }

    // M6d-6 PTY activity-based inject gate tests were removed in M6g
    // (2026-05-24). The gate fundamentally couldn't distinguish
    // "agent still streaming" from "agent just finished a turn", and
    // the latter case stranded wakes indefinitely (e2e #7). The gate
    // existed to protect against TTL-nudge pollution during M6d-5;
    // with TTL removed (M6e), the gate's protection has no use case
    // left and its edge case caused real bugs. See M6g commit for
    // details.
}
