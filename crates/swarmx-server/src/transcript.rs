//! Tail a worker's CLI session transcript (JSONL) and emit tool-level
//! [`SwarmEvent::AgentActivity`] — WITHOUT touching the worker.
//!
//! Both claude and codex write a structured JSONL log of every turn (used for
//! their own `resume`). We read that file; the worker is never configured,
//! hooked, or slowed. Compare the alternatives:
//!   - hooks run SYNCHRONOUSLY inside the worker's tool loop (~50ms tax/call),
//!   - parsing the PTY means decoding a human-facing ANSI screen (brittle).
//! Reading the file the CLI already writes costs the worker nothing and also
//! carries token usage (for future cost stats).
//!
//! The format has NO official spec, so parsing is deliberately LENIENT: a bad
//! line is skipped, an unknown shape yields no activity, never a panic. The
//! fixture tests at the bottom lock the exact fields we depend on — if a CLI
//! upgrade changes them, CI turns red instead of the feature silently emitting
//! nothing.

use swarmx_protocol::rest::AgentActivityRecord;
use swarmx_protocol::ws_swarm::{AgentState, SwarmEvent};
use swarmx_swarm::Swarm;
use serde_json::Value;
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// How often to re-read the file's tail. Tool calls are seconds apart in an
/// interactive worker, so sub-second latency isn't needed; polling avoids a
/// notify dependency + its debounce/rotation edge cases.
const POLL_INTERVAL: Duration = Duration::from_millis(700);
/// Give the CLI this long to create its session file before giving up.
///
/// Codex does not always materialize `sessions/YYYY/MM/DD/rollout-*.jsonl`
/// immediately at spawn. In real runs we have seen the file appear a few
/// minutes later, only after the worker has finished startup + its first
/// substantial turn. A short timeout makes the UI go blind for the whole run:
/// the worker spends real tokens, but Activity / Usage stay empty forever
/// because the tailer gave up before the file existed.
const LOCATE_TIMEOUT: Duration = Duration::from_secs(600);
const LOCATE_POLL: Duration = Duration::from_millis(500);
/// Cap a single tail read so one giant tool result can't balloon memory; we
/// only need the tool name + a short label, not the whole payload.
const MAX_READ: usize = 4 * 1024 * 1024;
/// Drop a single line longer than this (a huge embedded result) — it carries
/// no extra signal for our one-line labels and would bloat the buffer.
const MAX_LINE: usize = 512 * 1024;

#[derive(Clone, Copy)]
enum Flavor {
    Claude,
    Codex,
    Kimi,
}

/// Spawn a background task that tails `agent_id`'s session transcript and emits
/// `AgentActivity` for each tool call/result. `cli` is the plugin id
/// (e.g. "claude" / "codex" / "kimi"); `cwd` is the worker's canonical
/// workspace dir. No-op for an unknown CLI (no known transcript format).
pub fn spawn_tailer(
    swarm: Arc<Swarm>,
    store: Arc<swarmx_storage::Store>,
    registry: crate::registry::Registry,
    agent_id: String,
    cli: String,
    cwd: PathBuf,
    session_id: Option<String>,
) {
    let flavor = if cli.contains("codex") {
        Flavor::Codex
    } else if cli.contains("claude") {
        Flavor::Claude
    } else if cli.contains("kimi") {
        Flavor::Kimi
    } else {
        tracing::debug!(agent = %agent_id, cli = %cli, "transcript: unknown CLI flavor, not tailing");
        return;
    };
    tokio::spawn(async move {
        run(swarm, store, registry, &agent_id, flavor, &cwd, session_id.as_deref()).await;
    });
}

async fn run(
    swarm: Arc<Swarm>,
    store: Arc<swarmx_storage::Store>,
    registry: crate::registry::Registry,
    agent_id: &str,
    flavor: Flavor,
    cwd: &Path,
    session_id: Option<&str>,
) {
    let path = match locate(flavor, agent_id, cwd, session_id).await {
        Some(p) => p,
        None => {
            tracing::debug!(agent = %agent_id, "transcript: session file never appeared; giving up");
            return;
        }
    };
    tracing::info!(agent = %agent_id, path = %path.display(), "transcript: tailing");

    let mut rx = swarm.subscribe();
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut st = TailState::new(flavor);

    // High-water mark we've already persisted, so an idle tick (no new events)
    // doesn't fire a redundant UPDATE.
    let mut persisted: Option<i64> = None;
    // Set when this agent was flipped to `Error` while its process is still
    // alive — a SOFT error from the HealthScanner ("Not logged in") or the
    // first-response watchdog, NOT a process exit. We keep tailing through it
    // so that if the agent recovers (user runs /login in its terminal, or a
    // slow first turn finally produces output) we can clear the error latch.
    let mut soft_errored = false;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                st.poll(&path, &swarm, agent_id).await;
                let advanced =
                    persist_activity(&store, agent_id, st.last_emit_at, &mut persisted).await;
                let had_usage = !st.pending_usage.is_empty();
                persist_usage(&store, agent_id, &mut st.pending_usage).await;
                // Budget brake: fresh usage just landed — re-check the
                // workspace's estimated all-time cost against its cap and
                // trip (pause everyone + mark + notify) on crossing. Cheap
                // early-out inside when no cap is set.
                if had_usage {
                    crate::budget::maybe_trip_after_usage(&swarm, &registry, &store, agent_id).await;
                }
                // Recovery: a soft-errored agent that just produced real
                // activity is back to work — clear the persisted error and
                // publish a non-error state so the failure card / red dot /
                // red status strip all drop. Without this the Error latch is
                // one-way and a recovered agent shows dead forever.
                if soft_errored && advanced {
                    soft_errored = false;
                    if let Err(e) = store.clear_agent_error(agent_id.to_string()).await {
                        tracing::debug!(agent = %agent_id, ?e, "transcript: clear_agent_error failed");
                    }
                    tracing::info!(agent = %agent_id, "transcript: agent recovered after soft error; clearing error latch");
                    swarm.publish_event(SwarmEvent::AgentState {
                        agent_id: agent_id.to_string(),
                        state: AgentState::Idle,
                    });
                }
            }
            ev = rx.recv() => {
                match ev {
                    Ok(SwarmEvent::AgentState { agent_id: a, state })
                        if a == agent_id && matches!(state, AgentState::Exited | AgentState::Error) =>
                    {
                        // `Error` is overloaded: a non-zero shim exit (process
                        // dead, including kills) AND a soft auth/watchdog error
                        // (process alive) both publish it. Only stop tailing
                        // when the PROCESS is actually gone — otherwise a soft
                        // error would kill the tailer and freeze the agent's
                        // activity forever, turning a false alarm into an
                        // unrecoverable fake death. `Exited` is always terminal.
                        let dead = matches!(state, AgentState::Exited)
                            || store.agent_process_dead(agent_id.to_string()).await.unwrap_or(true);
                        if dead {
                            st.poll(&path, &swarm, agent_id).await; // final flush
                            persist_activity(&store, agent_id, st.last_emit_at, &mut persisted).await;
                            let had_usage = !st.pending_usage.is_empty();
                            persist_usage(&store, agent_id, &mut st.pending_usage).await;
                            if had_usage {
                                crate::budget::maybe_trip_after_usage(&swarm, &registry, &store, agent_id).await;
                            }
                            break;
                        }
                        // Soft error: keep tailing so we can detect recovery.
                        soft_errored = true;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    tracing::debug!(agent = %agent_id, "transcript: tailer stopped");
}

/// Persist `last_emit_at` to the agent row if it advanced past what we've
/// already written. Mutating `persisted` through `&mut` keeps the high-water
/// mark across poll ticks without a dead-store warning at the break path.
/// Returns `true` when it actually advanced (new activity this tick) — the
/// recovery path uses that to detect a soft-errored agent coming back to life.
async fn persist_activity(
    store: &swarmx_storage::Store,
    agent_id: &str,
    last_emit_at: Option<i64>,
    persisted: &mut Option<i64>,
) -> bool {
    if last_emit_at <= *persisted {
        return false;
    }
    if let Some(at) = last_emit_at {
        if let Err(e) = store.touch_agent_activity(agent_id.to_string(), at).await {
            tracing::debug!(agent = %agent_id, ?e, "transcript: touch_agent_activity failed");
        }
        *persisted = last_emit_at;
        return true;
    }
    false
}

/// Drain buffered token-usage events into the `agent_usage` table. Best-effort:
/// a failed insert is logged, not fatal — usage stats are observability, never
/// load-bearing for the wake/handoff machinery.
async fn persist_usage(
    store: &swarmx_storage::Store,
    agent_id: &str,
    pending: &mut Vec<UsageDelta>,
) {
    for u in pending.drain(..) {
        if let Err(e) = store
            .insert_agent_usage(
                agent_id.to_string(),
                u.model,
                u.input,
                u.output,
                u.cache_read,
                u.cache_write,
                u.at,
            )
            .await
        {
            tracing::debug!(agent = %agent_id, ?e, "transcript: insert_agent_usage failed");
        }
    }
}

// ── file location ──────────────────────────────────────────────────────────

async fn locate(
    flavor: Flavor,
    agent_id: &str,
    cwd: &Path,
    session_id: Option<&str>,
) -> Option<PathBuf> {
    let deadline = tokio::time::Instant::now() + LOCATE_TIMEOUT;
    loop {
        let found = match flavor {
            Flavor::Claude => claude_file(cwd, session_id),
            Flavor::Codex => codex_file(agent_id),
            Flavor::Kimi => kimi_file(cwd),
        };
        if found.is_some() {
            return found;
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(LOCATE_POLL).await;
    }
}

/// `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`, newest file.
fn claude_file(cwd: &Path, session_id: Option<&str>) -> Option<PathBuf> {
    let home = crate::runtime_path::swarmx_home()?;
    let dir = home
        .join(".claude")
        .join("projects")
        .join(encode_cwd(cwd));
    match session_id {
        // The exact file claude was told to write via `--session-id <uuid>`.
        // Avoids locking onto a STALE prior session's .jsonl that is still the
        // newest in this project dir before our claude has created its own —
        // the bug that surfaced re-spawning an orchestrator in one workspace.
        Some(sid) => {
            let p = dir.join(format!("{sid}.jsonl"));
            p.is_file().then_some(p)
        }
        // No forced id (defensive — claude always gets one now). Fall back to
        // the newest .jsonl in the dir.
        None => newest(&dir, false, &|p| {
            p.extension().and_then(|e| e.to_str()) == Some("jsonl")
        }),
    }
}

/// The EXACT transcript path claude will write for `--session-id <sid>` run in
/// `cwd`, WITHOUT gating on the file existing yet (unlike [`claude_file`]). Used
/// by the one-shot PTY query ([`crate::pty_query`]), which forces the session id
/// at spawn and then polls this path for the assistant's verbatim answer — the
/// transcript is the clean channel (no TUI ANSI/redraw artifacts) the PTY screen
/// isn't.
pub(crate) fn claude_transcript_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        Path::new(&home)
            .join(".claude")
            .join("projects")
            .join(encode_cwd(cwd))
            .join(format!("{session_id}.jsonl")),
    )
}

/// claude encodes the cwd into a directory name by replacing `/`, `.`, `_`, `\`
/// with `-` (case preserved). Lossy + collision-prone, so this is ONLY used
/// forward (known cwd → dir name), never reversed. Mirrors cc-trace's
/// `encode_path`.
fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '.' | '_' | '\\' => '-',
            other => other,
        })
        .collect()
}

/// `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`, newest. swarmx gives
/// each codex worker an isolated per-agent CODEX_HOME, so that tree holds only
/// this worker's session(s). New codex compresses cold files to `.jsonl.zst`;
/// the ACTIVE file stays plain `.jsonl`, which is what we tail.
fn codex_file(agent_id: &str) -> Option<PathBuf> {
    let home = crate::cli::codex::codex_per_agent_home_path(agent_id)?;
    let sessions = home.join("sessions");
    newest(&sessions, true, &|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
    })
}

/// kimi (Kimi Code): `<kimi_home>/sessions/wd_<slug>_<sha12>/<sessionId>/
/// agents/main/wire.jsonl` — the session's complete wire log (documented on
/// kimi's Data-locations page). The bucket hash is the first 12 hex chars of
/// sha256 over the CANONICAL session cwd (kimi canonicalizes it — macOS
/// /tmp → /private/tmp; verified against real buckets), so we canonicalize
/// too and match the bucket by hash SUFFIX: the slug segment's rules aren't
/// documented and the hash is the discriminator. Newest session dir inside
/// the bucket wins — each spawn starts a fresh kimi session, so that's ours.
/// (Two agents spawned into the same cwd in the same second could both land
/// on the newest — accepted; claude solves this with a pinned `--session-id`,
/// which kimi has no flag for.)
fn kimi_file(cwd: &Path) -> Option<PathBuf> {
    let home = crate::cli::kimi::kimi_home()?;
    kimi_file_at(&home, cwd)
}

fn kimi_file_at(home: &Path, cwd: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let hash = sha256_hex_12(canonical.to_string_lossy().as_bytes());
    let suffix = format!("_{hash}");
    let bucket = newest_dir_matching(&home.join("sessions"), &|name| {
        name.starts_with("wd_") && name.ends_with(&suffix)
    })?;
    // Newest session dir that has ALREADY materialized its wire.jsonl — a
    // brand-new session dir can precede the file by a beat, so scanning all
    // of them beats probing just the newest dir.
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(&bucket).ok()?.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let wire = e.path().join("agents").join("main").join("wire.jsonl");
        if !wire.is_file() {
            continue;
        }
        if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
            if best.as_ref().is_none_or(|(bt, _)| m > *bt) {
                best = Some((m, wire));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Newest (by mtime) immediate subdirectory of `dir` whose NAME matches
/// `pred`. Distinct from [`newest`] (which walks FILES); kimi's session
/// layout is two directory levels (`wd_<…>` bucket → `session_<id>`).
fn newest_dir_matching(dir: &Path, pred: &dyn Fn(&str) -> bool) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = e.file_name();
        if !pred(name.to_string_lossy().as_ref()) {
            continue;
        }
        if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
            if best.as_ref().is_none_or(|(bt, _)| m > *bt) {
                best = Some((m, e.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// First 12 hex chars of sha256 — kimi's session-bucket discriminator
/// (`wd_<slug>_<hash>`), computed over the canonical cwd string.
fn sha256_hex_12(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/// Newest (by mtime) file under `dir` matching `pred`. `recurse` walks
/// subdirectories (codex's date tree); claude is flat.
fn newest(dir: &Path, recurse: bool, pred: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if recurse {
                    stack.push(p);
                }
                continue;
            }
            if !pred(&p) {
                continue;
            }
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                if best.as_ref().is_none_or(|(bt, _)| m > *bt) {
                    best = Some((m, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

// ── tail + parse ─────────────────────────────────────────────────────────────

struct Pending {
    seq: u32,
    start_ms: i64,
    label: String,
}

struct TailState {
    flavor: Flavor,
    offset: u64,
    partial: Vec<u8>,
    pending: HashMap<String, Pending>,
    seq: u32,
    /// Unix-ms of the most recent event emitted this tailer's lifetime. The run
    /// loop diffs this against what it has persisted to decide whether to call
    /// `touch_agent_activity` after a poll (avoids a redundant UPDATE on idle
    /// ticks that produced no new events).
    last_emit_at: Option<i64>,
    /// Token-usage events parsed this poll, drained + persisted by the run loop.
    /// Buffered here (rather than written inline) so `poll` stays free of the
    /// store handle and the run loop owns all persistence.
    pending_usage: Vec<UsageDelta>,
}

impl TailState {
    fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            offset: 0,
            partial: Vec::new(),
            pending: HashMap::new(),
            seq: 0,
            last_emit_at: None,
            pending_usage: Vec::new(),
        }
    }

    async fn poll(&mut self, path: &Path, swarm: &Swarm, agent_id: &str) {
        let mut f = match tokio::fs::File::open(path).await {
            Ok(f) => f,
            Err(_) => return,
        };
        let len = match f.metadata().await {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        if len < self.offset {
            // File truncated / rotated — start over.
            self.offset = 0;
            self.partial.clear();
        }
        if len <= self.offset {
            return;
        }
        if f.seek(SeekFrom::Start(self.offset)).await.is_err() {
            return;
        }
        let want = ((len - self.offset) as usize).min(MAX_READ);
        let mut buf = vec![0u8; want];
        let n = match f.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return,
        };
        buf.truncate(n);
        self.offset += n as u64;
        self.partial.extend_from_slice(&buf);

        // Split out complete (newline-terminated) lines; keep the trailing
        // partial line for the next poll (it's still being written).
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=nl).collect();
            if line.len() > MAX_LINE {
                continue;
            }
            let line = &line[..line.len() - 1];
            let s = match std::str::from_utf8(line) {
                Ok(s) => s.trim(),
                Err(_) => continue,
            };
            if s.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(s) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let tools = match self.flavor {
                Flavor::Claude => parse_claude(&v),
                Flavor::Codex => parse_codex(&v),
                Flavor::Kimi => parse_kimi(&v),
            };
            for t in tools {
                self.emit(t, swarm, agent_id);
            }
            // Same line may also carry token usage (claude assistant turn /
            // codex token_count). Buffer it; the run loop persists.
            if let Some(u) = parse_usage(self.flavor, &v) {
                self.pending_usage.push(u);
            }
        }
        // A single absurdly long line with no newline would grow unbounded —
        // drop it.
        if self.partial.len() > MAX_LINE {
            self.partial.clear();
        }
    }

    fn emit(&mut self, t: ParsedTool, swarm: &Swarm, agent_id: &str) {
        let at = now_ms();
        // Every tool start/end is "the agent did something at `at`" — record the
        // high-water mark so the run loop can persist it (F3 stuck-detection).
        self.last_emit_at = Some(at);
        match t {
            ParsedTool::Start { tool_id, label } => {
                self.seq = self.seq.wrapping_add(1);
                let seq = self.seq;
                self.pending.insert(
                    tool_id,
                    Pending {
                        seq,
                        start_ms: at,
                        label: label.clone(),
                    },
                );
                emit_activity(swarm, agent_id, "running", label, seq, None, at);
            }
            ParsedTool::End {
                tool_id,
                ok,
                result,
            } => {
                // A result for a tool we never saw start (we attached
                // mid-session) is ignored — no running event to pair with.
                if let Some(p) = self.pending.remove(&tool_id) {
                    let dur = (at - p.start_ms).max(0) as u32;
                    let phase = if ok { "ok" } else { "error" };
                    // Enrich the start label with a result blurb: "Bash git
                    // status" → "Bash git status → 12 lines: On branch main".
                    let label = match result {
                        Some(r) => format!("{} → {}", p.label, r),
                        None => p.label,
                    };
                    emit_activity(swarm, agent_id, phase, label, p.seq, Some(dur), at);
                }
            }
            ParsedTool::Pulse { label } => {
                // Thinking heartbeat: fresh seq, system kind, zero duration —
                // a liveness timestamp, not a step with a lifecycle.
                self.seq = self.seq.wrapping_add(1);
                let seq = self.seq;
                emit_activity_kind(swarm, agent_id, "system", "ok", label, seq, None, at);
            }
        }
    }
}

/// Fan one tool-level activity out to BOTH sinks: the in-memory ring
/// (`Swarm::record_activity`, served by `GET /api/agent/:id/activity` for cold
/// backfill) and the live WS broadcast (`publish_event`). Centralised so a row
/// can never reach one sink but not the other — they share the same `seq`, so
/// the UI merges backfill + live by it.
/// Feed one tool-activity step into the live + persisted activity pipeline
/// (ring + SQLite + thought-trace derivation + WS broadcast). Used by the
/// transcript tailer for claude/codex, and by `POST /api/agent/:id/activity`
/// for engines we can't transcript-tail (opencode pushes its tool events here).
pub(crate) fn emit_activity(
    swarm: &Swarm,
    agent_id: &str,
    phase: &str,
    label: String,
    seq: u32,
    duration_ms: Option<u32>,
    at: i64,
) {
    emit_activity_kind(swarm, agent_id, "tool", phase, label, seq, duration_ms, at);
}

/// `emit_activity` with an explicit record kind. The thinking-step pulses
/// (kimi `llm.request` / codex `reasoning` / claude `thinking`) report as
/// kind="system" so the UI styles them apart from real tool calls.
pub(crate) fn emit_activity_kind(
    swarm: &Swarm,
    agent_id: &str,
    kind: &str,
    phase: &str,
    label: String,
    seq: u32,
    duration_ms: Option<u32>,
    at: i64,
) {
    swarm.record_activity(
        agent_id,
        AgentActivityRecord {
            agent_id: agent_id.to_string(),
            kind: kind.into(),
            label: label.clone(),
            phase: phase.to_string(),
            seq,
            duration_ms,
            at,
        },
    );
    swarm.publish_event(SwarmEvent::AgentActivity {
        agent_id: agent_id.to_string(),
        kind: kind.into(),
        label,
        phase: phase.to_string(),
        seq,
        duration_ms,
        at,
    });
}

enum ParsedTool {
    Start {
        tool_id: String,
        label: String,
    },
    End {
        tool_id: String,
        ok: bool,
        /// Short result blurb (line count + first-line snippet), appended to
        /// the label on completion. `None` when the result was empty/absent.
        result: Option<String>,
    },
    /// One-shot "the model is still thinking" heartbeat — the turn is alive
    /// between tool calls. Emitted as a system-level `ok` row (no pending
    /// pairing): its TIMESTAMP is what matters — it keeps `last_activity_at`
    /// fresh through long reasoning phases so the stall detector can tell
    /// "slow but alive" from "wedged" (the codex false-"卡住" lesson).
    Pulse { label: String },
}

/// claude: tool_use lives in an `assistant` row's `message.content[]`; the
/// matching tool_result lives in a later `user` row. The content-block `type`
/// is authoritative, so we don't gate on the row `type`.
fn parse_claude(v: &Value) -> Vec<ParsedTool> {
    let mut out = Vec::new();
    let content = match v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    {
        Some(c) => c,
        None => return out,
    };
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("thinking") => {
                // Claude's reasoning block — the turn is alive mid-think.
                // Timestamp-only signal (the thinking text itself is noise for
                // a one-line activity feed).
                out.push(ParsedTool::Pulse {
                    label: "思考中".into(),
                });
            }
            Some("tool_use") => {
                if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                    let label = summarize(name, block.get("input").unwrap_or(&Value::Null));
                    out.push(ParsedTool::Start {
                        tool_id: id.to_string(),
                        label,
                    });
                }
            }
            Some("tool_result") => {
                if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) {
                    let is_err = block
                        .get("is_error")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    let result = claude_result_text(block)
                        .as_deref()
                        .and_then(result_summary);
                    out.push(ParsedTool::End {
                        tool_id: id.to_string(),
                        ok: !is_err,
                        result,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// codex: a `response_item` row whose `payload.type` is a function/tool call or
/// its output. `arguments` is a JSON STRING (needs a second parse). codex
/// output carries no reliable error flag, so results are reported as `ok`.
fn parse_codex(v: &Value) -> Vec<ParsedTool> {
    let mut out = Vec::new();
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return out;
    }
    let payload = match v.get("payload") {
        Some(p) => p,
        None => return out,
    };
    match payload.get("type").and_then(|t| t.as_str()) {
        Some("reasoning") => {
            // codex's thinking step (`{"type":"reasoning", id:"rs_…"}`) — the
            // turn is alive mid-reason. Verified against real rollout JSONL.
            out.push(ParsedTool::Pulse {
                label: "思考中".into(),
            });
        }
        Some("function_call" | "custom_tool_call" | "local_shell_call") => {
            if let Some(id) = payload.get("call_id").and_then(|i| i.as_str()) {
                let name = payload
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool");
                let args = payload
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(Value::Null);
                out.push(ParsedTool::Start {
                    tool_id: id.to_string(),
                    label: summarize(name, &args),
                });
            }
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            if let Some(id) = payload.get("call_id").and_then(|i| i.as_str()) {
                // codex `output` is usually a raw string; tolerate an object
                // with a `content` string too. No reliable error flag → ok.
                let result = payload
                    .get("output")
                    .and_then(|o| match o {
                        Value::String(s) => Some(s.clone()),
                        other => other
                            .get("content")
                            .and_then(|c| c.as_str())
                            .map(str::to_string),
                    })
                    .as_deref()
                    .and_then(result_summary);
                out.push(ParsedTool::End {
                    tool_id: id.to_string(),
                    ok: true,
                    result,
                });
            }
        }
        _ => {}
    }
    out
}

/// kimi (Kimi Code): `context.append_loop_event` rows of the session's
/// `wire.jsonl` (documented Data-locations layout). Tool starts are
/// `event.type = "tool.call"` (`toolCallId`, `name`, `args`); completions are
/// `event.type = "tool.result"` (same id, `result.output`, failures flagged
/// by `result.isError`). Shapes verified against real 0.28 wire.jsonl files.
fn parse_kimi(v: &Value) -> Vec<ParsedTool> {
    let mut out = Vec::new();
    let ty = v.get("type").and_then(|t| t.as_str());
    // kimi's model-call row (`{"type":"llm.request","model":"k3",…}`) — the
    // turn is alive mid-think. One per model call (a few per turn), same
    // frequency class as tool calls. Verified against real 0.28 wire.jsonl.
    if ty == Some("llm.request") {
        let model = v
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("model");
        out.push(ParsedTool::Pulse {
            label: format!("思考中 · {model}"),
        });
        return out;
    }
    if ty != Some("context.append_loop_event") {
        return out;
    }
    let ev = match v.get("event") {
        Some(e) => e,
        None => return out,
    };
    match ev.get("type").and_then(|t| t.as_str()) {
        Some("tool.call") => {
            if let Some(id) = ev.get("toolCallId").and_then(|i| i.as_str()) {
                let name = ev.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let label = summarize(name, ev.get("args").unwrap_or(&Value::Null));
                out.push(ParsedTool::Start {
                    tool_id: id.to_string(),
                    label,
                });
            }
        }
        Some("tool.result") => {
            if let Some(id) = ev.get("toolCallId").and_then(|i| i.as_str()) {
                let result = ev.get("result").unwrap_or(&Value::Null);
                let is_err = result
                    .get("isError")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                let text = result.get("output").and_then(|o| o.as_str());
                out.push(ParsedTool::End {
                    tool_id: id.to_string(),
                    ok: !is_err,
                    result: text.and_then(result_summary),
                });
            }
        }
        _ => {}
    }
    out
}

/// One token-usage event parsed from a transcript line. Buffered on the
/// `TailState` and persisted by the run loop into `agent_usage`.
struct UsageDelta {
    model: Option<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    at: i64,
}

fn usage_num(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}

/// Extract token usage from a transcript line, if it carries any.
///   claude — `assistant` row's `message.usage` (per turn).
///   codex  — `event_msg`/`token_count`'s `info.last_token_usage` (per-turn
///            delta; the cumulative `total_token_usage` would double-count).
/// Returns None for lines with no usage or an all-zero usage block.
fn parse_usage(flavor: Flavor, v: &Value) -> Option<UsageDelta> {
    match flavor {
        Flavor::Claude => {
            let msg = v.get("message")?;
            let usage = msg.get("usage")?;
            let input = usage_num(usage, "input_tokens");
            let output = usage_num(usage, "output_tokens");
            let cache_write = usage_num(usage, "cache_creation_input_tokens");
            let cache_read = usage_num(usage, "cache_read_input_tokens");
            if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
                return None;
            }
            Some(UsageDelta {
                model: msg
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(str::to_string),
                input,
                output,
                cache_read,
                cache_write,
                at: now_ms(),
            })
        }
        Flavor::Codex => {
            if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
                return None;
            }
            let payload = v.get("payload")?;
            if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
                return None;
            }
            let info = payload.get("info")?;
            let last = info.get("last_token_usage")?;
            // Codex/OpenAI semantics DIFFER from Anthropic: `input_tokens` is the
            // TOTAL prompt size and `cached_input_tokens` is a SUBSET already
            // counted inside it (verified against real ~/.codex transcripts:
            // input+output == total_tokens, cached <= input). cost_of treats
            // input and cache_read as DISJOINT (it bills each at its own rate),
            // matching Anthropic where they're reported separately. So we must
            // subtract the cached portion out of `input` here — otherwise the
            // cached tokens get billed twice (once at full input rate, once at
            // the cache-read rate), badly overstating codex cost on the common
            // high-cache-hit turn.
            let raw_input = usage_num(last, "input_tokens");
            let output = usage_num(last, "output_tokens");
            let cache_read = usage_num(last, "cached_input_tokens");
            // Saturating: never let a malformed line where cached > input
            // produce a negative (and thus a negative cost).
            let input = (raw_input - cache_read).max(0);
            if input == 0 && output == 0 && cache_read == 0 {
                return None;
            }
            Some(UsageDelta {
                model: info
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(str::to_string),
                input,
                output,
                cache_read,
                cache_write: 0,
                at: now_ms(),
            })
        }
        Flavor::Kimi => {
            // kimi: `usage.record` rows of the session wire.jsonl. Gate on
            // `usageScope == "turn"`: it's the ONLY scope seen in 1315 real
            // samples, and skipping a future cumulative-scope row is how we
            // dodge the double-count codex's `total_token_usage` would cause.
            if v.get("type").and_then(|t| t.as_str()) != Some("usage.record") {
                return None;
            }
            if v.get("usageScope").and_then(|s| s.as_str()) != Some("turn") {
                return None;
            }
            let usage = v.get("usage")?;
            // Field semantics INFERRED from real rows (no official spec): the
            // components are reported separately, Anthropic-style (e.g.
            // `{inputOther: 362, inputCacheRead: 23296}` on a warm-cache turn)
            // — so input/cache_read/cache_write are treated DISJOINT, the
            // same convention `cost_of` uses.
            let input = usage_num(usage, "inputOther");
            let output = usage_num(usage, "output");
            let cache_read = usage_num(usage, "inputCacheRead");
            let cache_write = usage_num(usage, "inputCacheCreation");
            if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
                return None;
            }
            Some(UsageDelta {
                model: v
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(str::to_string),
                input,
                output,
                cache_read,
                cache_write,
                at: now_ms(),
            })
        }
    }
}

/// MCP tools arrive as `mcp__<server>__<action>`; show just `<action>` so a
/// member row / activity line isn't dominated by the `mcp__swarmx-swarm__`
/// prefix. Non-MCP names (Bash, Edit, …) pass through unchanged.
fn prettify_tool_name(name: &str) -> &str {
    match name.strip_prefix("mcp__") {
        Some(rest) => rest.rsplit("__").next().unwrap_or(rest),
        None => name,
    }
}

/// One-line human label: tool name + a salient string argument (a path,
/// command, pattern, …), whitespace-collapsed and truncated. Falls back to
/// just the name when no salient arg is present.
fn summarize(name: &str, input: &Value) -> String {
    let name = prettify_tool_name(name);
    const KEYS: &[&str] = &[
        "file_path",
        "path",
        "command",
        "cmd",
        "pattern",
        "query",
        "url",
        "notebook_path",
        "prompt",
    ];
    let detail = input.as_object().and_then(|obj| {
        KEYS.iter().find_map(|k| match obj.get(*k) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
    });
    match detail {
        Some(s) => format!("{} {}", name, shorten(&collapse_ws(&s))),
        None => name.to_string(),
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shorten(s: &str) -> String {
    const MAX: usize = 48;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= MAX {
        s.to_string()
    } else {
        let head: String = chars[..MAX].iter().collect();
        format!("{head}…")
    }
}

/// Pull the text out of a claude `tool_result` block — its `content` is either
/// a raw string or an array of `{type:"text", text}` parts.
fn claude_result_text(block: &Value) -> Option<String> {
    match block.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let mut buf = String::new();
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(s) = p.get("text").and_then(|t| t.as_str()) {
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(s);
                    }
                }
            }
            (!buf.is_empty()).then_some(buf)
        }
        _ => None,
    }
}

/// Short, scannable result blurb appended to a finished tool's label, e.g.
/// `12 lines: On branch main`. Generic — claude/codex don't expose exit codes
/// reliably here — but a line count + first-line snippet beats a bare `ok`.
fn result_summary(text: &str) -> Option<String> {
    let nonempty: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let first = *nonempty.first()?;
    let snippet = shorten(&collapse_ws(first));
    if nonempty.len() > 1 {
        Some(format!("{} lines: {}", nonempty.len(), snippet))
    } else {
        Some(snippet)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(v: &[ParsedTool]) -> &ParsedTool {
        assert_eq!(v.len(), 1, "expected exactly one parsed tool");
        &v[0]
    }

    // ── claude fixtures (real-shape lines; lock the fields we read) ──────────

    #[test]
    fn claude_tool_use_makes_a_start_with_label() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"/Users/x/src/foo.rs"}}]}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_claude(&v)) {
            ParsedTool::Start { tool_id, label } => {
                assert_eq!(tool_id, "toolu_1");
                assert!(label.starts_with("Edit "), "label was {label:?}");
                assert!(label.contains("foo.rs"), "label was {label:?}");
            }
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn claude_tool_result_ok_and_error() {
        let ok_line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"done"}],"is_error":false}]}}"#;
        let v: Value = serde_json::from_str(ok_line).unwrap();
        match one(&parse_claude(&v)) {
            ParsedTool::End {
                tool_id,
                ok,
                result,
            } => {
                assert_eq!(tool_id, "toolu_1");
                assert!(ok);
                assert_eq!(result.as_deref(), Some("done"));
            }
            _ => panic!("expected End"),
        }
        let err_line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"boom","is_error":true}]}}"#;
        let v: Value = serde_json::from_str(err_line).unwrap();
        match one(&parse_claude(&v)) {
            ParsedTool::End { ok, result, .. } => {
                assert!(!ok);
                assert_eq!(result.as_deref(), Some("boom"));
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn claude_tool_result_summarizes_multiline() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"text","text":"On branch main\nnothing to commit\nclean"}],"is_error":false}]}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_claude(&v)) {
            ParsedTool::End { result, .. } => {
                assert_eq!(result.as_deref(), Some("3 lines: On branch main"));
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn claude_non_tool_rows_yield_nothing() {
        for line in [
            r#"{"type":"system","content":"hi"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"thinking out loud"}]}}"#,
            r#"{"type":"summary"}"#,
        ] {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(
                parse_claude(&v).is_empty(),
                "line should yield nothing: {line}"
            );
        }
    }

    // ── codex fixtures ───────────────────────────────────────────────────────

    #[test]
    fn codex_function_call_and_output() {
        let call = r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"git status\"}","call_id":"call_1"}}"#;
        let v: Value = serde_json::from_str(call).unwrap();
        match one(&parse_codex(&v)) {
            ParsedTool::Start { tool_id, label } => {
                assert_eq!(tool_id, "call_1");
                assert!(label.starts_with("exec_command "), "label was {label:?}");
                assert!(label.contains("git status"), "label was {label:?}");
            }
            _ => panic!("expected Start"),
        }
        let out = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"on branch main"}}"#;
        let v: Value = serde_json::from_str(out).unwrap();
        match one(&parse_codex(&v)) {
            ParsedTool::End {
                tool_id,
                ok,
                result,
            } => {
                assert_eq!(tool_id, "call_1");
                assert!(ok);
                assert_eq!(result.as_deref(), Some("on branch main"));
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn codex_non_tool_rows_yield_nothing() {
        for line in [
            r#"{"type":"session_meta","payload":{"id":"x"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant"}}"#,
        ] {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(
                parse_codex(&v).is_empty(),
                "line should yield nothing: {line}"
            );
        }
    }

    // ── lenience + encoding ──────────────────────────────────────────────────

    #[test]
    fn garbage_never_panics() {
        for v in [
            serde_json::json!({}),
            serde_json::json!({"type": 42}),
            serde_json::json!({"message": {"content": "not-an-array"}}),
            serde_json::json!({"type": "response_item"}), // codex: no payload
        ] {
            let _ = parse_claude(&v);
            let _ = parse_codex(&v);
        }
    }

    #[test]
    fn prettify_mcp_tool_name_strips_prefix() {
        assert_eq!(
            prettify_tool_name("mcp__swarmx-swarm__swarm_list_messages"),
            "swarm_list_messages"
        );
        assert_eq!(prettify_tool_name("Bash"), "Bash");
        assert_eq!(prettify_tool_name("Edit"), "Edit");
    }

    #[test]
    fn encode_cwd_matches_claude_rule() {
        assert_eq!(
            encode_cwd(Path::new("/Users/wdx/opc/swarmx")),
            "-Users-wdx-opc-swarmx"
        );
        // `/.swarmx` → `--swarmx` (both `/` and `.` map to `-`).
        assert_eq!(
            encode_cwd(Path::new("/Users/wdx/.swarmx/workspaces/claude-106ea14e")),
            "-Users-wdx--swarmx-workspaces-claude-106ea14e"
        );
    }

    #[test]
    fn summarize_truncates_and_collapses() {
        let long = "a ".repeat(100);
        let v = serde_json::json!({ "command": long });
        let label = summarize("Bash", &v);
        assert!(label.starts_with("Bash "));
        assert!(label.chars().count() <= "Bash ".len() + 49); // 48 + ellipsis
                                                              // no embedded newlines/double spaces
        let v = serde_json::json!({ "command": "git\n  status" });
        assert_eq!(summarize("Bash", &v), "Bash git status");
    }

    // ── usage parsing ────────────────────────────────────────────────────────

    #[test]
    fn claude_usage_parsed_from_assistant_turn() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-4","usage":{"input_tokens":120,"output_tokens":45,"cache_creation_input_tokens":10,"cache_read_input_tokens":900}}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let u = parse_usage(Flavor::Claude, &v).expect("usage");
        assert_eq!(u.input, 120);
        assert_eq!(u.output, 45);
        assert_eq!(u.cache_write, 10);
        assert_eq!(u.cache_read, 900);
        assert_eq!(u.model.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn codex_usage_parsed_from_last_token_usage() {
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":80,"output_tokens":20,"cached_input_tokens":5},"total_token_usage":{"input_tokens":9999}}}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let u = parse_usage(Flavor::Codex, &v).expect("usage");
        // per-turn delta, NOT the cumulative total
        // Codex `input_tokens` (80) is the TOTAL prompt and INCLUDES the 5 cached
        // tokens; we subtract them so input(75) and cache_read(5) are disjoint and
        // cost_of doesn't bill the cached tokens twice. 75 + 5 == 80 (the raw total).
        assert_eq!(u.input, 75);
        assert_eq!(u.output, 20);
        assert_eq!(u.cache_read, 5);
    }

    #[test]
    fn codex_cached_never_exceeds_input_goes_negative() {
        // Defensive: a malformed line where cached > input must clamp input to 0,
        // never produce a negative token count (which would yield negative cost).
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":5,"output_tokens":10,"cached_input_tokens":9999}}}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let u = parse_usage(Flavor::Codex, &v).expect("usage");
        assert_eq!(u.input, 0);
        assert_eq!(u.output, 10);
        assert_eq!(u.cache_read, 9999);
    }

    #[test]
    fn non_usage_lines_yield_none() {
        for (flavor, line) in [
            (
                Flavor::Claude,
                r#"{"type":"user","message":{"content":[]}}"#,
            ),
            (
                Flavor::Claude,
                r#"{"type":"assistant","message":{"usage":{"input_tokens":0,"output_tokens":0}}}"#,
            ),
            (
                Flavor::Codex,
                r#"{"type":"response_item","payload":{"type":"function_call"}}"#,
            ),
        ] {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(parse_usage(flavor, &v).is_none(), "should be None: {line}");
        }
    }

    // ── kimi fixtures (real 0.28 wire.jsonl shapes; lock the fields we read) ──

    #[test]
    fn kimi_tool_call_makes_a_start_with_pretty_label() {
        let line = r##"{"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"tool_1","turnId":"0","step":1,"toolCallId":"tool_1","name":"mcp__swarmx-swarm__swarm_write_blackboard","args":{"path":"ws/main/task.ledger.md","content":"# Task Ledger"},"time":1784555566575}}"##;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_kimi(&v)) {
            ParsedTool::Start { tool_id, label } => {
                assert_eq!(tool_id, "tool_1");
                // mcp prefix stripped, salient arg (path) in the label.
                assert!(label.starts_with("swarm_write_blackboard "), "label was {label:?}");
                assert!(label.contains("task.ledger.md"), "label was {label:?}");
            }
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn kimi_builtin_tool_call_uses_command_arg() {
        let line = r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"tool_2","name":"Bash","args":{"command":"ls -la"},"time":1}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_kimi(&v)) {
            ParsedTool::Start { tool_id, label } => {
                assert_eq!(tool_id, "tool_2");
                assert_eq!(label, "Bash ls -la");
            }
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn kimi_tool_result_makes_an_ok_end() {
        let line = r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"tool_1","result":{"output":"total 0\ndrwxr-xr-x ."},"time":1}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_kimi(&v)) {
            ParsedTool::End {
                tool_id,
                ok,
                result,
            } => {
                assert_eq!(tool_id, "tool_1");
                assert!(ok);
                assert!(result.clone().unwrap().contains("total 0"));
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn kimi_tool_result_is_error_makes_a_failed_end() {
        let line = r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"tool_9","result":{"isError":true,"output":"permission denied"},"time":1}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_kimi(&v)) {
            ParsedTool::End { tool_id, ok, .. } => {
                assert_eq!(tool_id, "tool_9");
                assert!(!ok, "isError must map to a failed End");
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn kimi_non_loop_rows_yield_nothing() {
        for line in [
            r#"{"type":"metadata"}"#,
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}]}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","turnId":"0","step":1},"time":1}"#,
        ] {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(parse_kimi(&v).is_empty(), "should be empty: {line}");
        }
    }

    #[test]
    fn kimi_usage_record_maps_disjoint_fields() {
        let line = r#"{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":362,"output":125,"inputCacheRead":23296,"inputCacheCreation":0},"usageScope":"turn","time":1}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let u = parse_usage(Flavor::Kimi, &v).expect("usage");
        assert_eq!(u.model.as_deref(), Some("kimi-code/k3"));
        assert_eq!(u.input, 362);
        assert_eq!(u.output, 125);
        assert_eq!(u.cache_read, 23296);
        assert_eq!(u.cache_write, 0);
    }

    #[test]
    fn kimi_usage_skips_non_turn_scope_and_zero_rows() {
        // A future cumulative scope must not double-count.
        let line = r#"{"type":"usage.record","model":"m","usage":{"inputOther":1,"output":1,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"total","time":1}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(parse_usage(Flavor::Kimi, &v).is_none());
        // All-zero turn row is noise, not usage.
        let line = r#"{"type":"usage.record","usage":{"inputOther":0,"output":0,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(parse_usage(Flavor::Kimi, &v).is_none());
    }

    #[test]
    fn kimi_bucket_hash_matches_real_workdirkey() {
        // Real bucket on disk: `wd_demo3_2b1181f934bc` for
        // `/private/tmp/swarmx-teststack/demo3` — locks the sha256[:12] rule.
        assert_eq!(
            sha256_hex_12(b"/private/tmp/swarmx-teststack/demo3"),
            "2b1181f934bc"
        );
    }

    #[test]
    fn kimi_file_discovers_newest_session_with_wire() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let canonical = std::fs::canonicalize(&cwd).unwrap();
        let hash = sha256_hex_12(canonical.to_string_lossy().as_bytes());
        let bucket = home.join("sessions").join(format!("wd_ws_{hash}"));
        // Older session WITH wire, newer session WITHOUT (still initializing),
        // plus a decoy bucket with a different hash.
        let s_old = bucket.join("session_old");
        let s_new = bucket.join("session_new");
        std::fs::create_dir_all(s_old.join("agents/main")).unwrap();
        std::fs::create_dir_all(&s_new).unwrap();
        std::fs::write(s_old.join("agents/main/wire.jsonl"), "{}\n").unwrap();
        let decoy = home.join("sessions").join("wd_ws_000000000000");
        std::fs::create_dir_all(decoy.join("session_decoy/agents/main")).unwrap();
        std::fs::write(
            decoy.join("session_decoy/agents/main/wire.jsonl"),
            "{}\n",
        )
        .unwrap();

        let found = kimi_file_at(home, &cwd).expect("wire discovered");
        // The wire-less newer session is SKIPPED in favour of the older one
        // that actually has wire.jsonl; the decoy bucket (wrong hash) ignored.
        assert_eq!(found, s_old.join("agents/main/wire.jsonl"));
    }

    #[test]
    fn kimi_llm_request_is_a_thinking_pulse() {
        let line = r#"{"type":"llm.request","kind":"loop","provider":"kimi","model":"k3","modelAlias":"kimi-code/k3","messageCount":3,"turnStep":"0.2","time":1784555693131}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_kimi(&v)) {
            ParsedTool::Pulse { label } => {
                assert!(label.contains("k3"), "label was {label:?}");
            }
            _ => panic!("expected Pulse"),
        }
    }

    #[test]
    fn codex_reasoning_is_a_thinking_pulse() {
        // Real rollout shape (content is encrypted; the event itself is the signal).
        let line = r#"{"timestamp":"2026-07-04T15:44:48.194Z","type":"response_item","payload":{"type":"reasoning","id":"rs_0aac","summary":[],"encrypted_content":"gAAAA"}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        match one(&parse_codex(&v)) {
            ParsedTool::Pulse { label } => assert!(!label.is_empty()),
            _ => panic!("expected Pulse"),
        }
    }

    #[test]
    fn claude_thinking_block_is_a_thinking_pulse() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"","signature":"Ev"},{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"/x"}}]}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let tools = parse_claude(&v);
        assert_eq!(tools.len(), 2, "thinking pulse + tool_use start");
        assert!(matches!(tools[0], ParsedTool::Pulse { .. }));
        assert!(matches!(tools[1], ParsedTool::Start { .. }));
    }
}
