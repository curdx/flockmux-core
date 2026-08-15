//! The `Store` API. Every public method hops to `tokio::spawn_blocking`
//! so callers can `.await` it from inside an axum handler without blocking
//! the runtime.

use crate::connection::Customizer;
use crate::models::{
    AgentActivityRow, AgentRecord, BlackboardOpRecord, FusionBatchRecord, GoalEvidenceRecord,
    GoalRecord,
    ListMessagesOpts,
    MessageRecord, NewAgent, NewBlackboardOp, NewFusionBatch, NewGoal, NewGoalEvidence, NewMessage,
    NewRecording,
    NewSpellRun, NewThoughtTrace, NewThoughtTraceEvent, NewThread, NewWorker, NewWorkspace,
    NewWorkspaceRoot, OrphanedAgentProcess, RecordingRecord, SpellRunRecord, ThoughtTraceRecord,
    ThreadRecord, WorkerRecord, WorkspaceRecord, WorkspaceRootRecord,
};
use crate::schema;
use anyhow::{Context, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, Error as SqliteError, ErrorCode, Row};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_THOUGHT_TRACE_SUMMARY_STEPS: usize = 12;
const THOUGHT_TRACE_RECENT_DONE_APPEND_WINDOW_MS: i64 = 30_000;

/// SELECT list shared by both message-search paths (FTS MATCH and the CJK
/// substring fallback) — the row shape is identical, only FROM/WHERE differ.
const MESSAGE_SEARCH_COLS: &str = "\
    m.id, m.from_agent, m.to_agent, m.kind, m.body, m.sent_at, \
    m.delivered_at, m.read_at, m.in_reply_to, m.thread_id, m.meta, \
    tt.id, tt.trigger_message_id, tt.response_message_id, tt.agent_id, \
    tt.workspace_id, tt.thread_id, tt.status, tt.started_at, \
    tt.completed_at, tt.summary_json, tt.updated_at";

/// True if `e` is a `SQLITE_BUSY` / `SQLITE_LOCKED` failure — the only
/// errors `with_busy_retry` re-runs on.
fn is_busy(e: &SqliteError) -> bool {
    matches!(
        e,
        SqliteError::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Rewrite an arbitrary user search string into a safe FTS5 MATCH expression.
///
/// FTS5 treats `*` `:` `(` `)` `-` `^` `"` `+`, the keywords AND/OR/NOT/NEAR,
/// and bareword quirks as *query operators*. Passing the raw user string to
/// `MATCH` therefore lets a malformed input raise a SQLite syntax error (which
/// would otherwise leak out as an HTTP 500). We defuse this by emitting a list
/// of double-quoted phrase tokens: inside a quoted FTS5 string every special
/// character is literal token text, so no input can form an operator.
///
/// Tokenization mirrors the `unicode61` tokenizer used by `messages_fts`: we
/// break on every non-alphanumeric character (Unicode-aware, so non-ASCII
/// token text survives the rewrite). Each token becomes `"token"`, with any
/// embedded `"` escaped per the FTS5 spec by doubling it (`""`); tokens are
/// joined by spaces (implicit AND). Returns an empty string when the input
/// has no searchable characters — the caller treats that as "no results"
/// rather than running an (also-invalid) empty MATCH.
fn sanitize_fts5_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for token in raw.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push('"');
        // A bare-alphanumeric token can't contain `"`, but escape defensively
        // in case the tokenization rule is ever loosened.
        for ch in token.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    }
    out
}

/// True when the query contains characters from scripts the FTS5 `unicode61`
/// tokenizer cannot split (CJK ideographs, kana, hangul): unicode61 merges a
/// run of them into ONE token, so `MATCH '"共享"'` can never hit a body like
/// 「把结论放到共享区」 — Chinese search silently returned zero rows (the
/// agent then wrongly concluded "nobody discussed this"). Such queries must
/// take the substring path in `search_messages` instead. (FTS5 trigram was
/// considered and rejected: it only matches ≥3-char substrings, so 2-char
/// words — the common case in Chinese — would still miss.)
fn needs_substring_match(raw: &str) -> bool {
    raw.chars().any(|c| {
        matches!(c,
            '\u{3400}'..='\u{4DBF}'   // CJK Extension A
            | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
            | '\u{3040}'..='\u{30FF}' // hiragana / katakana
            | '\u{AC00}'..='\u{D7AF}' // hangul syllables
        )
    })
}

/// True if `e` is a UNIQUE/constraint failure — used by `create_workspace` to
/// retry on a (rare) generated-slug collision rather than surfacing a 500.
fn is_constraint_violation(e: &SqliteError) -> bool {
    matches!(
        e,
        SqliteError::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation
    )
}

/// Run a DB op, re-running it on SQLITE_BUSY / SQLITE_LOCKED that `busy_timeout`
/// didn't absorb (WAL checkpoint / snapshot edge cases). Safe to retry: a BUSY
/// is returned BEFORE the statement/transaction takes effect, so no partial
/// write is left behind. Up to 5 attempts, backoff 10/20/40/80 ms.
///
/// Owns the pool checkout (a fresh connection per attempt) so the `op` closure
/// receives a `&mut Connection` and never touches `pool.get()` directly.
fn with_busy_retry<T>(
    pool: &Pool<SqliteConnectionManager>,
    mut op: impl FnMut(&mut Connection) -> rusqlite::Result<T>,
) -> Result<T> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt: u32 = 0;
    loop {
        let mut conn = pool.get()?;
        match op(&mut conn) {
            Ok(v) => return Ok(v),
            Err(e) if is_busy(&e) && attempt + 1 < MAX_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(10u64 << attempt));
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// What a single [`Store::prune_expired`] pass removed. All counts are rows
/// actually deleted; `recording_files_removed` is the subset of pruned
/// recordings whose `.cast` file was also unlinked from disk (best-effort).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    pub blackboard_ops: usize,
    pub messages: usize,
    pub recordings: usize,
    pub recording_files_removed: usize,
    /// Per-step usage rows older than the window (the highest-frequency table —
    /// one row per agent turn; unbounded without this).
    pub agent_usage: usize,
    /// Activity-log rows older than the window (also append-heavy).
    pub agent_activities: usize,
}

impl PruneStats {
    /// True if nothing was deleted — lets the caller skip a log line.
    pub fn is_empty(&self) -> bool {
        self.blackboard_ops == 0
            && self.messages == 0
            && self.recordings == 0
            && self.agent_usage == 0
            && self.agent_activities == 0
    }
}

/// Thread-safe handle to the SQLite store. Cheap to clone — wraps an `Arc`
/// over the r2d2 pool.
#[derive(Clone)]
pub struct Store {
    pool: Arc<Pool<SqliteConnectionManager>>,
}

fn thought_trace_from_row(
    row: &Row<'_>,
    offset: usize,
) -> rusqlite::Result<Option<ThoughtTraceRecord>> {
    let id: Option<String> = row.get(offset)?;
    let Some(id) = id else {
        return Ok(None);
    };
    Ok(Some(ThoughtTraceRecord {
        id,
        trigger_message_id: row.get(offset + 1)?,
        response_message_id: row.get(offset + 2)?,
        agent_id: row.get(offset + 3)?,
        workspace_id: row.get(offset + 4)?,
        thread_id: row.get(offset + 5)?,
        status: row.get(offset + 6)?,
        started_at: row.get(offset + 7)?,
        completed_at: row.get(offset + 8)?,
        summary_json: row.get(offset + 9)?,
        updated_at: row.get(offset + 10)?,
    }))
}

fn message_with_trace_from_row(row: &Row<'_>) -> rusqlite::Result<MessageRecord> {
    Ok(MessageRecord {
        id: row.get(0)?,
        from_agent: row.get(1)?,
        to_agent: row.get(2)?,
        kind: row.get(3)?,
        body: row.get(4)?,
        sent_at: row.get(5)?,
        delivered_at: row.get(6)?,
        read_at: row.get(7)?,
        in_reply_to: row.get(8)?,
        thread_id: row.get(9)?,
        meta: row
            .get::<_, Option<String>>(10)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        thought_trace: thought_trace_from_row(row, 11)?,
    })
}

fn insert_thought_trace_events(
    conn: &Connection,
    trace_id: &str,
    events: &[NewThoughtTraceEvent],
) -> rusqlite::Result<()> {
    for ev in events {
        let meta_txt = ev.meta.as_ref().map(|v| v.to_string());
        conn.execute(
            "INSERT INTO thought_trace_events (trace_id, phase, label, source, at, meta) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![trace_id, ev.phase, ev.label, ev.source, ev.at, meta_txt],
        )?;
    }
    Ok(())
}

fn parse_thought_trace_steps(summary_json: &str) -> Vec<crate::models::ThoughtTraceStep> {
    serde_json::from_str::<Vec<crate::models::ThoughtTraceStep>>(summary_json).unwrap_or_default()
}

fn event_to_thought_trace_step(ev: &NewThoughtTraceEvent) -> crate::models::ThoughtTraceStep {
    crate::models::ThoughtTraceStep {
        phase: ev.phase.clone(),
        label: ev.label.clone(),
        source: ev.source.clone(),
        at: ev.at,
    }
}

fn merge_thought_trace_steps(
    existing_json: &str,
    incoming: impl IntoIterator<Item = crate::models::ThoughtTraceStep>,
) -> String {
    let mut steps = parse_thought_trace_steps(existing_json);
    for step in incoming {
        if step.label.trim().is_empty() {
            continue;
        }
        if steps
            .iter()
            .any(|s| s.phase == step.phase && s.source == step.source && s.label == step.label)
        {
            continue;
        }
        steps.push(step);
    }
    if steps.len() > MAX_THOUGHT_TRACE_SUMMARY_STEPS {
        let keep_from = steps.len() - MAX_THOUGHT_TRACE_SUMMARY_STEPS;
        steps = steps.split_off(keep_from);
    }
    serde_json::to_string(&steps).unwrap_or_else(|_| "[]".into())
}

fn select_thought_trace(conn: &Connection, id: &str) -> rusqlite::Result<ThoughtTraceRecord> {
    conn.query_row(
        "SELECT id, trigger_message_id, response_message_id, agent_id, workspace_id, thread_id, \
                status, started_at, completed_at, summary_json, updated_at \
         FROM thought_traces WHERE id = ?1",
        params![id],
        |row| {
            Ok(ThoughtTraceRecord {
                id: row.get(0)?,
                trigger_message_id: row.get(1)?,
                response_message_id: row.get(2)?,
                agent_id: row.get(3)?,
                workspace_id: row.get(4)?,
                thread_id: row.get(5)?,
                status: row.get(6)?,
                started_at: row.get(7)?,
                completed_at: row.get(8)?,
                summary_json: row.get(9)?,
                updated_at: row.get(10)?,
            })
        },
    )
}

/// Append a raw suffix to a path (e.g. `swarmx.db` + `-wal`). Unlike
/// `Path::with_extension`, this does NOT replace the existing extension — the
/// SQLite sidecar files are `<db>-wal` / `<db>-shm`, not `<db>.wal`.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Best-effort millis since the unix epoch, for unique archive filenames.
fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Run `PRAGMA quick_check` on an existing database. If it cannot be opened or
/// fails the check (truncated WAL after power loss, `NOTADB`, …), archive the
/// file plus its `-wal`/`-shm` sidecars so the caller recreates an empty
/// database in its place instead of crashing on boot with no path to recovery.
/// Returns `Err` only if the archival itself fails — leaving a corrupt file in
/// place would be worse than surfacing that error.
fn integrity_guard(path: &Path) -> Result<()> {
    let healthy = match Connection::open(path) {
        Ok(conn) => match conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)) {
            Ok(result) => result == "ok",
            Err(e) => {
                tracing::error!(error = %e, "数据库完整性检查失败，按损坏处理");
                false
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "数据库无法打开，按损坏处理");
            false
        }
    };

    if healthy {
        return Ok(());
    }

    let archived = with_suffix(path, &format!(".corrupt-{}", unix_millis()));
    std::fs::rename(path, &archived)
        .with_context(|| format!("archive corrupt database to {}", archived.display()))?;
    for ext in ["-wal", "-shm"] {
        let side = with_suffix(path, ext);
        if side.exists() {
            let _ = std::fs::rename(&side, with_suffix(&archived, ext));
        }
    }
    tracing::warn!(
        archived = %archived.display(),
        "数据库已损坏：已归档损坏文件并将在原路径重建空库（历史数据保留在归档文件中）"
    );
    Ok(())
}

/// `VACUUM INTO` a consistent snapshot before applying migrations, so a failed
/// or buggy migration can be rolled back to the pre-migration state. Keeps only
/// the few most recent snapshots.
fn snapshot_before_migration(conn: &Connection, path: &Path, target: i64) -> Result<()> {
    let backup = with_suffix(path, &format!(".pre-v{target}.bak"));
    // A leftover snapshot from a previous boot at the same target is stale.
    let _ = std::fs::remove_file(&backup);
    // VACUUM INTO takes a string literal, not a bind parameter; escape quotes.
    let escaped = backup.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}'"))
        .with_context(|| format!("VACUUM INTO snapshot before migrating to v{target}"))?;
    tracing::info!(backup = %backup.display(), "迁移前已生成数据库快照");
    prune_old_snapshots(path, 3);
    Ok(())
}

/// Keep only the `keep` newest `<db>.pre-v*.bak` snapshots; delete older ones.
/// Best-effort — any IO error just leaves the extra snapshots in place.
fn prune_old_snapshots(path: &Path, keep: usize) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|s| s.to_str())) else {
        return;
    };
    let prefix = format!("{name}.pre-v");
    let mut snaps: Vec<(std::time::SystemTime, PathBuf)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let n = p.file_name()?.to_str()?;
                if n.starts_with(&prefix) && n.ends_with(".bak") {
                    let mtime = e.metadata().ok()?.modified().ok()?;
                    Some((mtime, p))
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => return,
    };
    if snaps.len() <= keep {
        return;
    }
    snaps.sort_by_key(|(t, _)| *t);
    let remove = snaps.len() - keep;
    for (_, p) in snaps.into_iter().take(remove) {
        let _ = std::fs::remove_file(p);
    }
}

impl Store {
    /// Open a store at `path`, running migrations if needed. Parent dir must
    /// already exist.
    pub async fn open(path: &Path) -> Result<Self> {
        let path: PathBuf = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::open_blocking(&path))
            .await
            .context("spawn_blocking open")?
    }

    fn open_blocking(path: &Path) -> Result<Self> {
        // Corruption guard: an existing database that fails `PRAGMA quick_check`
        // is archived aside so the pool below recreates an empty one, rather
        // than crashing the server on boot with no path to recovery.
        if path.exists() {
            integrity_guard(path).context("database integrity guard")?;
        }

        let manager = SqliteConnectionManager::file(path);
        // `min_idle(0)` keeps r2d2 from eagerly opening every connection at
        // `build()` time. Eager parallel opens race on the very first
        // `PRAGMA journal_mode=WAL`, producing "database is locked" noise in
        // the boot log. Lazy creation lets WAL get set by the first checkout
        // and persisted to the file before any sibling connection appears.
        let pool = Pool::builder()
            // WAL allows many concurrent readers + one writer; SQLite
            // connections are cheap (a few KB each). 8 was the global ceiling on
            // DB concurrency — under a multi-agent swarm (each agent's transcript
            // tailer + message writes + usage rows) it gets saturated and new
            // checkouts block in `pool.get()`. 16 gives read headroom; writes
            // still serialize on the single WAL writer regardless.
            .max_size(16)
            .min_idle(Some(0))
            .connection_customizer(Box::new(Customizer))
            .build(manager)
            .context("build r2d2 pool")?;
        // Run migrations on a dedicated connection (not the pool — we want
        // exclusive access for the schema bring-up).
        let mut conn = pool.get().context("checkout for migrations")?;

        // Snapshot before any schema change so a failed/buggy migration can be
        // rolled back. Only when an existing, non-empty database actually has
        // migrations pending (a fresh DB has nothing worth snapshotting).
        let current = schema::current_version(&conn).unwrap_or(0);
        let latest = schema::latest_migration();
        if current > 0 && current < latest {
            snapshot_before_migration(&conn, path, latest)
                .context("snapshot database before migration")?;
        }

        schema::run_migrations(&mut conn).context("run migrations")?;
        drop(conn);
        Ok(Self {
            pool: Arc::new(pool),
        })
    }

    // ── agents ────────────────────────────────────────────────────────────

    pub async fn record_agent_spawn(&self, rec: NewAgent) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
            conn.execute(
                "INSERT INTO agents (id, cli, role, workspace, spawned_at, workspace_id, spell_run_id, thread_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    rec.id,
                    rec.cli,
                    rec.role,
                    rec.workspace,
                    rec.spawned_at,
                    rec.workspace_id,
                    rec.spell_run_id,
                    rec.thread_id,
                ],
            )?;
            Ok(())
        }))
        .await
        .context("spawn_blocking record_agent_spawn")?
    }

    pub async fn record_agent_kill(&self, id: String, at_ms: i64) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE agents SET killed_at = ?2 WHERE id = ?1 AND killed_at IS NULL",
                    params![id, at_ms],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking record_agent_kill")?
    }

    pub async fn record_shim_ready(&self, id: String, at_ms: i64) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE agents SET shim_ready_at = ?2 WHERE id = ?1 AND shim_ready_at IS NULL",
                    params![id, at_ms],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking record_shim_ready")?
    }

    pub async fn record_shim_exit(&self, id: String, code: i32, at_ms: i64) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE agents SET shim_exit_at = ?2, shim_exit_code = ?3 \
                 WHERE id = ?1 AND shim_exit_at IS NULL",
                    params![id, at_ms, code],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking record_shim_exit")?
    }

    /// Persist the shim's OS pid + (unix) process-group id — the process
    /// identity the startup orphan reaper needs to really kill the tree after
    /// a server crash/SIGKILL (migration 0029). First-write-wins: the
    /// `shim_pid IS NULL` guard makes re-records a no-op so a stale writer
    /// can't clobber a fresher value. Returns true when this call did the
    /// write; false when the row doesn't exist yet (the reaper retries on its
    /// next sweep — `record_agent_spawn` can race us) or was already recorded.
    pub async fn record_agent_shim_pid(
        &self,
        id: String,
        pid: i64,
        pgid: Option<i64>,
    ) -> Result<bool> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<bool> {
                let n = conn.execute(
                    "UPDATE agents SET shim_pid = ?2, shim_pgid = ?3 \
                     WHERE id = ?1 AND shim_pid IS NULL",
                    params![id, pid, pgid],
                )?;
                Ok(n > 0)
            })
        })
        .await
        .context("spawn_blocking record_agent_shim_pid")?
    }

    /// Persist an "alive but can't work" failure reason (auth/quota banner from
    /// the HealthScanner, or the first-response watchdog firing). Read back by
    /// `list_agents` into `AgentInfo.last_error` so the UI can re-render an
    /// honest failure card on a cold load — the live `AgentState::Error` WS
    /// event is lossy with no resume. Last-write-wins (a later failure
    /// overwrites an earlier one); a successful re-spawn is a new agent row, so
    /// we never need to clear this.
    pub async fn record_agent_error(
        &self,
        id: String,
        reason: String,
        kind: &str,
        at_ms: i64,
    ) -> Result<()> {
        let pool = self.pool.clone();
        let kind = kind.to_string();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE agents SET last_error = ?2, last_error_kind = ?3, last_error_at = ?4 \
                 WHERE id = ?1",
                    params![id, reason, kind, at_ms],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking record_agent_error")?
    }

    /// Clear a previously-recorded `last_error` once an agent recovers in place
    /// — e.g. the user ran `/login` in the failed agent's own terminal and it
    /// resumed working, or the first-response watchdog false-fired on a slow
    /// first turn and the agent then produced real activity. Without this the
    /// Error latch is one-way: the failure card, red member dot, and red status
    /// strip would persist forever even though the agent is actively working
    /// (the transcript tailer detects the recovery and calls this).
    pub async fn clear_agent_error(&self, id: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE agents SET last_error = NULL, last_error_kind = NULL, \
                     last_error_at = NULL WHERE id = ?1",
                    params![id],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking clear_agent_error")?
    }

    /// True when the agent's PROCESS is gone — killed, or its shim recorded an
    /// exit. Lets the transcript tailer tell a terminal `AgentState::Error`
    /// (non-zero shim exit, including kills) from a SOFT error (HealthScanner /
    /// first-response watchdog flipped the agent to Error while its process is
    /// still alive). The watcher persists `shim_exit_at` BEFORE publishing the
    /// exit-driven Error, so by the time the tailer sees that event this read is
    /// already true — no race. A soft error leaves both columns NULL, so the
    /// tailer keeps tailing and can observe the agent's recovery.
    pub async fn agent_process_dead(&self, id: String) -> Result<bool> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<bool> {
                conn.query_row(
                    "SELECT (killed_at IS NOT NULL OR shim_exit_at IS NOT NULL) \
                     FROM agents WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, i64>(0).map(|b| b != 0),
                )
                // No row ⇒ treat as gone (nothing left to tail).
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(true),
                    other => Err(other),
                })
            })
        })
        .await
        .context("spawn_blocking agent_process_dead")?
    }

    /// First-response watchdog probe: true when the agent is still live (not
    /// killed, no shim exit, no already-recorded error) yet has produced NO
    /// observable sign of life — no persisted message of its own, no tool-level
    /// activity, and no token usage. That combination, a generous window after
    /// ShimReady, means it's wedged (an auth prompt we didn't needle, a hung
    /// hook, an MCP that never settled). Token usage is the decisive liveness
    /// signal that keeps a legitimately-slow first turn (an orchestrator reading
    /// the repo before greeting) from being misflagged: tokens accrue the moment
    /// it talks to the model, even with zero messages. Crucially this covers the
    /// orchestrator, which the transcript tailer doesn't watch — the exact gap
    /// that let "暂无消息" sit forever behind a fake green dot. One query.
    pub async fn agent_silent_since_ready(&self, id: String) -> Result<bool> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<bool> {
                conn.query_row(
                    "SELECT (a.killed_at IS NULL AND a.shim_exit_at IS NULL \
                          AND a.last_error IS NULL AND a.last_activity_at IS NULL \
                          AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.from_agent = a.id) \
                          AND NOT EXISTS (SELECT 1 FROM agent_usage u WHERE u.agent_id = a.id)) \
                     FROM agents a WHERE a.id = ?1",
                    params![id],
                    |row| row.get::<_, i64>(0).map(|b| b != 0),
                )
                // No such agent row (already torn down) ⇒ nothing to flag.
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(false),
                    other => Err(other),
                })
            })
        })
        .await
        .context("spawn_blocking agent_silent_since_ready")?
    }

    /// Stalled-agent probe backing the NeedsYou inbox's "该醒没醒" signal: of
    /// the given agent ids, which ones are ALIVE (shim reported ready, not
    /// killed, no shim exit) yet have unread mailbox mail whose OLDEST message
    /// has waited at least `silence_ms`, with no persisted activity in that
    /// same window (`last_activity_at` NULL or older than the cutoff)?
    ///
    /// This is deliberately a much tighter signal than the 0.3.0
    /// "suspected-stuck" heuristic that cried wolf (a codex mid-turn emitting
    /// exec events got flagged): an idle worker waiting for work has NO
    /// unread mail and never matches; a busy worker keeps `last_activity_at`
    /// fresh and never matches. Matching requires something to be WAITING for
    /// the agent while the agent shows no sign of life — the exact shape of
    /// a broken wake chain (wake-check never fired / the push-wake injection
    /// never landed).
    ///
    /// Batch variant (one query per chunk, N+1 not allowed in the agents
    /// list path). `MIN(sent_at)` over unread mail is NULL when nothing is
    /// unread, and `NULL <= cutoff` is false — so "no unread" needs no
    /// separate clause.
    pub async fn stalled_agents_with_unread(
        &self,
        ids: Vec<String>,
        now_ms: i64,
        silence_ms: i64,
    ) -> Result<std::collections::HashSet<String>> {
        if ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let cutoff = now_ms - silence_ms;
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<std::collections::HashSet<String>> {
                    // 900 ids + 2 cutoff binds stay under SQLite's 999-var cap.
                    const CHUNK: usize = 900;
                    let mut stalled = std::collections::HashSet::new();
                    for chunk in ids.chunks(CHUNK) {
                        let placeholders = vec!["?"; chunk.len()].join(",");
                        let sql = format!(
                            "SELECT a.id FROM agents a \
                             WHERE a.id IN ({placeholders}) \
                               AND a.shim_ready_at IS NOT NULL \
                               AND a.killed_at IS NULL \
                               AND a.shim_exit_at IS NULL \
                               AND (a.last_activity_at IS NULL OR a.last_activity_at <= ?) \
                               AND (SELECT MIN(m.sent_at) FROM messages m \
                                    WHERE m.to_agent = a.id AND m.read_at IS NULL) <= ?"
                        );
                        let mut stmt = conn.prepare(&sql)?;
                        let mut binds: Vec<rusqlite::types::Value> =
                            chunk.iter().map(|id| id.clone().into()).collect();
                        binds.push(cutoff.into());
                        binds.push(cutoff.into());
                        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                            row.get::<_, String>(0)
                        })?;
                        for r in rows {
                            stalled.insert(r?);
                        }
                    }
                    Ok(stalled)
                },
            )
        })
        .await
        .context("spawn_blocking stalled_agents_with_unread")?
    }

    /// Persist the agent's most recent tool-level activity time, called by the
    /// transcript tailer. Monotonic — only ever moves forward — so an
    /// out-of-order or stale poll can't rewind a fresher timestamp. Cheap UPDATE
    /// on a single indexed row; the tailer throttles to at most one per poll.
    pub async fn touch_agent_activity(&self, id: String, at_ms: i64) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE agents SET last_activity_at = ?2 \
                 WHERE id = ?1 AND (last_activity_at IS NULL OR last_activity_at < ?2)",
                    params![id, at_ms],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking touch_agent_activity")?
    }

    /// Upsert one tool-level activity step (P1: persist the in-memory ring so
    /// `GET /api/agent/:id/activity` survives a cold load / reconnect / restart).
    /// Keyed by (agent_id, seq): a `running` row is replaced in place by its
    /// later ok/error, matching the ring's collapse-by-seq.
    pub async fn insert_agent_activity(
        &self,
        agent_id: String,
        seq: u32,
        kind: String,
        label: String,
        phase: String,
        duration_ms: Option<u32>,
        at: i64,
    ) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "INSERT INTO agent_activities \
                       (agent_id, seq, kind, label, phase, duration_ms, at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                     ON CONFLICT(agent_id, seq) DO UPDATE SET \
                       kind = excluded.kind, label = excluded.label, \
                       phase = excluded.phase, duration_ms = excluded.duration_ms, \
                       at = excluded.at",
                    params![agent_id, seq, kind, label, phase, duration_ms, at],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking insert_agent_activity")?
    }

    /// Persisted activity for `agent_id` — the last `limit` steps. Order is not
    /// guaranteed meaningful (the server merges by seq); callers that want
    /// oldest-first should sort. Empty for agents that never acted.
    pub async fn recent_agent_activities(
        &self,
        agent_id: &str,
        limit: i64,
    ) -> Result<Vec<AgentActivityRow>> {
        let pool = self.pool.clone();
        let agent_id = agent_id.to_string();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<AgentActivityRow>> {
                let mut stmt = conn.prepare(
                    "SELECT agent_id, seq, kind, label, phase, duration_ms, at \
                     FROM agent_activities WHERE agent_id = ?1 \
                     ORDER BY seq DESC LIMIT ?2",
                )?;
                let v = stmt
                    .query_map(params![agent_id, limit], |row| {
                        Ok(AgentActivityRow {
                            agent_id: row.get(0)?,
                            seq: row.get(1)?,
                            kind: row.get(2)?,
                            label: row.get(3)?,
                            phase: row.get(4)?,
                            duration_ms: row.get(5)?,
                            at: row.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(v)
            })
        })
        .await
        .context("spawn_blocking recent_agent_activities")?
    }

    /// Record one token-usage event (claude assistant turn / codex token_count).
    /// Append-only — one row per event; aggregation happens at query time. No
    /// dedup needed: the tailer reads each JSONL line exactly once (offset only
    /// advances), so a usage line can't be inserted twice.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_agent_usage(
        &self,
        agent_id: String,
        model: Option<String>,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
        at_ms: i64,
    ) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
            conn.execute(
                "INSERT INTO agent_usage \
                   (agent_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    agent_id,
                    model,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    at_ms
                ],
            )?;
            Ok(())
        }))
        .await
        .context("spawn_blocking insert_agent_usage")?
    }

    /// Usage aggregated by model (descending by total tokens). Cost is applied
    /// by the server's pricing table, not here.
    /// `ws = Some(id)` scopes to one workspace by joining `agents` (rows in
    /// `agent_usage` carry only a loose `agent_id`). `None` keeps the original
    /// no-JOIN aggregate so orphaned usage rows still count in the global total.
    pub async fn usage_by_model(
        &self,
        ws: Option<String>,
    ) -> Result<Vec<crate::models::UsageByModel>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<crate::models::UsageByModel>> {
            fn map_row(row: &rusqlite::Row) -> rusqlite::Result<crate::models::UsageByModel> {
                Ok(crate::models::UsageByModel {
                    model: row.get(0)?,
                    input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    cache_read_tokens: row.get(3)?,
                    cache_write_tokens: row.get(4)?,
                    events: row.get(5)?,
                    // MAX(input+cache_read+cache_write) per event ≈ peak context
                    // sent in a single request. COALESCE so an empty group is 0.
                    context_peak: row.get(6)?,
                })
            }
            let rows = if let Some(ws) = &ws {
                let mut stmt = conn.prepare(
                    "SELECT u.model, \
                            SUM(u.input_tokens), SUM(u.output_tokens), \
                            SUM(u.cache_read_tokens), SUM(u.cache_write_tokens), COUNT(*), \
                            COALESCE(MAX(u.input_tokens + u.cache_read_tokens + u.cache_write_tokens), 0) \
                     FROM agent_usage u JOIN agents a ON a.id = u.agent_id \
                     WHERE a.workspace_id = ?1 GROUP BY u.model \
                     ORDER BY SUM(u.input_tokens) + SUM(u.output_tokens) DESC",
                )?;
                let v = stmt.query_map(params![ws], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                v
            } else {
                let mut stmt = conn.prepare(
                    "SELECT model, \
                            SUM(input_tokens), SUM(output_tokens), \
                            SUM(cache_read_tokens), SUM(cache_write_tokens), COUNT(*), \
                            COALESCE(MAX(input_tokens + cache_read_tokens + cache_write_tokens), 0) \
                     FROM agent_usage GROUP BY model \
                     ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC",
                )?;
                let v = stmt.query_map([], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                v
            };
            Ok(rows)
        }))
        .await
        .context("spawn_blocking usage_by_model")?
    }

    /// Usage aggregated by UTC calendar day, oldest→newest, last `days` days.
    /// `ws = Some(id)` scopes to one workspace (see `usage_by_model`).
    pub async fn usage_by_day(
        &self,
        days: i64,
        ws: Option<String>,
    ) -> Result<Vec<crate::models::UsageByDay>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<Vec<crate::models::UsageByDay>> {
                    fn map_row(row: &rusqlite::Row) -> rusqlite::Result<crate::models::UsageByDay> {
                        Ok(crate::models::UsageByDay {
                            day: row.get(0)?,
                            input_tokens: row.get(1)?,
                            output_tokens: row.get(2)?,
                            cache_read_tokens: row.get(3)?,
                            cache_write_tokens: row.get(4)?,
                        })
                    }
                    let limit = days.max(1);
                    // Take the MOST RECENT `limit` days, not the earliest: order
                    // DESC + LIMIT to grab the latest window, then reverse back to
                    // ascending so the trend chart reads old→new. (Was ASC+LIMIT,
                    // which pinned long-time users' charts to their first N days.)
                    let mut rows = if let Some(ws) = &ws {
                        let mut stmt = conn.prepare(
                            "SELECT date(u.at/1000, 'unixepoch') AS day, \
                            SUM(u.input_tokens), SUM(u.output_tokens), \
                            SUM(u.cache_read_tokens), SUM(u.cache_write_tokens) \
                     FROM agent_usage u JOIN agents a ON a.id = u.agent_id \
                     WHERE a.workspace_id = ?1 GROUP BY day ORDER BY day DESC LIMIT ?2",
                        )?;
                        let v = stmt
                            .query_map(params![ws, limit], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        v
                    } else {
                        let mut stmt = conn.prepare(
                            "SELECT date(at/1000, 'unixepoch') AS day, \
                            SUM(input_tokens), SUM(output_tokens), \
                            SUM(cache_read_tokens), SUM(cache_write_tokens) \
                     FROM agent_usage GROUP BY day ORDER BY day DESC LIMIT ?1",
                        )?;
                        let v = stmt
                            .query_map(params![limit], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        v
                    };
                    rows.reverse();
                    Ok(rows)
                },
            )
        })
        .await
        .context("spawn_blocking usage_by_day")?
    }

    /// Usage aggregated by agent (descending by total tokens).
    /// `ws = Some(id)` scopes to one workspace (see `usage_by_model`).
    pub async fn usage_by_agent(
        &self,
        ws: Option<String>,
    ) -> Result<Vec<crate::models::UsageByAgent>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<Vec<crate::models::UsageByAgent>> {
                    fn map_row(
                        row: &rusqlite::Row,
                    ) -> rusqlite::Result<crate::models::UsageByAgent> {
                        Ok(crate::models::UsageByAgent {
                            agent_id: row.get(0)?,
                            role: row.get(1)?,
                            workspace_id: row.get(2)?,
                            thread_id: row.get(3)?,
                            input_tokens: row.get(4)?,
                            output_tokens: row.get(5)?,
                            cache_read_tokens: row.get(6)?,
                            cache_write_tokens: row.get(7)?,
                            events: row.get(8)?,
                        })
                    }
                    let rows = if let Some(ws) = &ws {
                        let mut stmt = conn.prepare(
                    "SELECT u.agent_id, MAX(a.role), MAX(a.workspace_id), MAX(a.thread_id), \
                            SUM(u.input_tokens), SUM(u.output_tokens), \
                            SUM(u.cache_read_tokens), SUM(u.cache_write_tokens), COUNT(*) \
                     FROM agent_usage u JOIN agents a ON a.id = u.agent_id \
                     WHERE a.workspace_id = ?1 GROUP BY u.agent_id \
                     ORDER BY SUM(u.input_tokens) + SUM(u.output_tokens) DESC",
                )?;
                        let v = stmt
                            .query_map(params![ws], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        v
                    } else {
                        let mut stmt = conn.prepare(
                    "SELECT u.agent_id, MAX(a.role), MAX(a.workspace_id), MAX(a.thread_id), \
                            SUM(u.input_tokens), SUM(u.output_tokens), \
                            SUM(u.cache_read_tokens), SUM(u.cache_write_tokens), COUNT(*) \
                     FROM agent_usage u LEFT JOIN agents a ON a.id = u.agent_id \
                     GROUP BY u.agent_id \
                     ORDER BY SUM(u.input_tokens) + SUM(u.output_tokens) DESC",
                )?;
                        let v = stmt
                            .query_map([], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        v
                    };
                    Ok(rows)
                },
            )
        })
        .await
        .context("spawn_blocking usage_by_agent")?
    }

    /// List all workers as Kanban tasks, joined with their agent lifecycle and
    /// blackboard handoff signals. Ordered newest-first. The effective status is
    /// derived server-side (`routes::tasks`) from these raw fields.
    /// `workspace_id = Some(id)` scopes the board to one workspace; `None`
    /// returns every workspace's workers (the global view).
    pub async fn list_tasks(
        &self,
        workspace_id: Option<String>,
    ) -> Result<Vec<crate::models::TaskRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<crate::models::TaskRecord>> {
            fn map_row(row: &rusqlite::Row) -> rusqlite::Result<crate::models::TaskRecord> {
                Ok(crate::models::TaskRecord {
                    agent_id: row.get(0)?,
                    parent_agent_id: row.get(1)?,
                    role_label: row.get(2)?,
                    role_slug: row.get(3)?,
                    handoff_signal: row.get(4)?,
                    task_status: row.get(5)?,
                    spawned_at: row.get(6)?,
                    killed_at: row.get(7)?,
                    shim_exit_code: row.get(8)?,
                    last_activity_at: row.get(9)?,
                    workspace_id: row.get(10)?,
                    thread_id: row.get(11)?,
                    handoff_done: row.get::<_, i64>(12)? != 0,
                    error_present: row.get::<_, i64>(13)? != 0,
                })
            }
            // A handoff key "exists" only when its LATEST op is not a delete
            // tombstone: blackboard_ops is append-only, so a put-then-deleted
            // key keeps its write row forever. A bare EXISTS would report a
            // deleted deliverable as still done/blocked. Take MAX(id) per path
            // and require op != 'delete' — same "does this key exist"
            // definition as `blackboard_paths_present` and the readiness gate.
            let cols = "SELECT w.agent_id, w.parent_agent_id, w.role_label, w.role_slug, \
                        w.handoff_signal, w.task_status, w.spawned_at, \
                        a.killed_at, a.shim_exit_code, a.last_activity_at, \
                        a.workspace_id, a.thread_id, \
                        (w.handoff_signal IS NOT NULL AND w.handoff_signal <> '' \
                         AND EXISTS (SELECT 1 FROM blackboard_ops b \
                                     WHERE b.path = w.handoff_signal AND b.op != 'delete' \
                                     AND b.id = (SELECT MAX(id) FROM blackboard_ops c WHERE c.path = b.path))) AS handoff_done, \
                        (w.handoff_signal IS NOT NULL AND w.handoff_signal <> '' \
                         AND EXISTS (SELECT 1 FROM blackboard_ops b \
                                     WHERE b.path = w.handoff_signal || '.error' AND b.op != 'delete' \
                                     AND b.id = (SELECT MAX(id) FROM blackboard_ops c WHERE c.path = b.path))) AS error_present \
                 FROM workers w JOIN agents a ON a.id = w.agent_id";
            let rows = if let Some(ws) = &workspace_id {
                let sql = format!("{cols} WHERE a.workspace_id = ?1 ORDER BY w.spawned_at DESC");
                let mut stmt = conn.prepare(&sql)?;
                let v = stmt.query_map(params![ws], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                v
            } else {
                let sql = format!("{cols} ORDER BY w.spawned_at DESC");
                let mut stmt = conn.prepare(&sql)?;
                let v = stmt.query_map([], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
                v
            };
            Ok(rows)
        }))
        .await
        .context("spawn_blocking list_tasks")?
    }

    /// Set (or clear, with `None`) the human task-status override on a worker.
    ///
    /// Returns `true` if a matching worker row was updated, `false` if the
    /// `agent_id` does not exist (so callers can answer 404 instead of lying
    /// about success).
    pub async fn set_task_status(&self, agent_id: String, status: Option<String>) -> Result<bool> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<bool> {
                let affected = conn.execute(
                    "UPDATE workers SET task_status = ?2 WHERE agent_id = ?1",
                    params![agent_id, status],
                )?;
                Ok(affected > 0)
            })
        })
        .await
        .context("spawn_blocking set_task_status")?
    }

    // ── goals ─────────────────────────────────────────────────────────────

    pub async fn upsert_goal(&self, rec: NewGoal) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "INSERT INTO goals \
                     (id, workspace_id, thread_id, objective, success_criteria, status, \
                      budget_tokens, created_at, updated_at, completed_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                     ON CONFLICT(id) DO UPDATE SET \
                       objective = excluded.objective, \
                       success_criteria = excluded.success_criteria, \
                       status = excluded.status, \
                       budget_tokens = excluded.budget_tokens, \
                       updated_at = excluded.updated_at, \
                       completed_at = excluded.completed_at",
                    params![
                        rec.id,
                        rec.workspace_id,
                        rec.thread_id,
                        rec.objective,
                        rec.success_criteria,
                        rec.status,
                        rec.budget_tokens,
                        rec.created_at,
                        rec.updated_at,
                        rec.completed_at,
                    ],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking upsert_goal")?
    }

    pub async fn list_goals(
        &self,
        workspace_id: Option<String>,
        thread_id: Option<Option<String>>,
    ) -> Result<Vec<GoalRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<GoalRecord>> {
                fn map_row(row: &rusqlite::Row) -> rusqlite::Result<GoalRecord> {
                    Ok(GoalRecord {
                        id: row.get(0)?,
                        workspace_id: row.get(1)?,
                        thread_id: row.get(2)?,
                        objective: row.get(3)?,
                        success_criteria: row.get(4)?,
                        status: row.get(5)?,
                        budget_tokens: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                        completed_at: row.get(9)?,
                    })
                }

                let base = "SELECT id, workspace_id, thread_id, objective, success_criteria, \
                            status, budget_tokens, created_at, updated_at, completed_at \
                            FROM goals";
                match (workspace_id.as_deref(), thread_id.as_ref()) {
                    (Some(ws), Some(Some(tid))) => {
                        let sql = format!("{base} WHERE workspace_id = ?1 AND thread_id = ?2 ORDER BY updated_at DESC");
                        let mut stmt = conn.prepare(&sql)?;
                        let out = stmt
                            .query_map(params![ws, tid], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>();
                        out
                    }
                    (Some(ws), Some(None)) => {
                        let sql = format!("{base} WHERE workspace_id = ?1 AND thread_id IS NULL ORDER BY updated_at DESC");
                        let mut stmt = conn.prepare(&sql)?;
                        let out = stmt
                            .query_map(params![ws], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>();
                        out
                    }
                    (Some(ws), None) => {
                        let sql = format!("{base} WHERE workspace_id = ?1 ORDER BY updated_at DESC");
                        let mut stmt = conn.prepare(&sql)?;
                        let out = stmt
                            .query_map(params![ws], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>();
                        out
                    }
                    (None, _) => {
                        let sql = format!("{base} ORDER BY updated_at DESC");
                        let mut stmt = conn.prepare(&sql)?;
                        let out = stmt
                            .query_map([], map_row)?
                            .collect::<rusqlite::Result<Vec<_>>>();
                        out
                    }
                }
            })
        })
        .await
        .context("spawn_blocking list_goals")?
    }

    pub async fn update_goal_status(
        &self,
        id: String,
        status: String,
        updated_at: i64,
        completed_at: Option<i64>,
    ) -> Result<bool> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<bool> {
                let changed = conn.execute(
                    "UPDATE goals SET status = ?2, updated_at = ?3, completed_at = ?4 WHERE id = ?1",
                    params![id, status, updated_at, completed_at],
                )?;
                Ok(changed > 0)
            })
        })
        .await
        .context("spawn_blocking update_goal_status")?
    }

    pub async fn add_goal_evidence(&self, rec: NewGoalEvidence) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "INSERT INTO goal_evidence \
                     (id, goal_id, kind, summary, source_agent_id, blackboard_path, command, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        rec.id,
                        rec.goal_id,
                        rec.kind,
                        rec.summary,
                        rec.source_agent_id,
                        rec.blackboard_path,
                        rec.command,
                        rec.created_at,
                    ],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking add_goal_evidence")?
    }

    pub async fn list_goal_evidence(
        &self,
        goal_id: String,
        limit: usize,
    ) -> Result<Vec<GoalEvidenceRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<GoalEvidenceRecord>> {
                let limit = limit.clamp(1, 200) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, goal_id, kind, summary, source_agent_id, blackboard_path, command, created_at \
                     FROM goal_evidence WHERE goal_id = ?1 ORDER BY created_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![goal_id, limit], |row| {
                    Ok(GoalEvidenceRecord {
                        id: row.get(0)?,
                        goal_id: row.get(1)?,
                        kind: row.get(2)?,
                        summary: row.get(3)?,
                        source_agent_id: row.get(4)?,
                        blackboard_path: row.get(5)?,
                        command: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
        })
        .await
        .context("spawn_blocking list_goal_evidence")?
    }

    // ── cron jobs ─────────────────────────────────────────────────────────

    pub async fn record_cron_job(&self, rec: crate::models::CronJobRecord) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
            conn.execute(
                "INSERT INTO cron_jobs (id, workspace_id, name, cron_expr, prompt, enabled, created_at, last_run_at, tz_offset_minutes) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rec.id,
                    rec.workspace_id,
                    rec.name,
                    rec.cron_expr,
                    rec.prompt,
                    rec.enabled as i64,
                    rec.created_at,
                    rec.last_run_at,
                    rec.tz_offset_minutes,
                ],
            )?;
            Ok(())
        }))
        .await
        .context("spawn_blocking record_cron_job")?
    }

    pub async fn list_cron_jobs(&self) -> Result<Vec<crate::models::CronJobRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<crate::models::CronJobRecord>> {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, name, cron_expr, prompt, enabled, created_at, last_run_at, tz_offset_minutes \
                 FROM cron_jobs ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(crate::models::CronJobRecord {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    name: row.get(2)?,
                    cron_expr: row.get(3)?,
                    prompt: row.get(4)?,
                    enabled: row.get::<_, i64>(5)? != 0,
                    created_at: row.get(6)?,
                    last_run_at: row.get(7)?,
                    tz_offset_minutes: row.get(8)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        }))
        .await
        .context("spawn_blocking list_cron_jobs")?
    }

    pub async fn delete_cron_job(&self, id: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking delete_cron_job")?
    }

    pub async fn set_cron_enabled(&self, id: String, enabled: bool) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE cron_jobs SET enabled = ?2 WHERE id = ?1",
                    params![id, enabled as i64],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking set_cron_enabled")?
    }

    pub async fn touch_cron_run(&self, id: String, at_ms: i64) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE cron_jobs SET last_run_at = ?2 WHERE id = ?1",
                    params![id, at_ms],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking touch_cron_run")?
    }

    /// Edit a job's mutable fields (everything except id/created_at/enabled/
    /// last_run_at). Returns rows affected (0 = no such id). `enabled` is left
    /// untouched so editing doesn't silently re-enable a paused job.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_cron_job(
        &self,
        id: String,
        workspace_id: String,
        name: String,
        cron_expr: String,
        prompt: String,
        tz_offset_minutes: i32,
    ) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                conn.execute(
                    "UPDATE cron_jobs SET workspace_id = ?2, name = ?3, cron_expr = ?4, \
                     prompt = ?5, tz_offset_minutes = ?6 WHERE id = ?1",
                    params![id, workspace_id, name, cron_expr, prompt, tz_offset_minutes],
                )
            })
        })
        .await
        .context("spawn_blocking update_cron_job")?
    }

    /// Disable every cron job bound to a workspace. Called when a workspace is
    /// deleted so its schedules stop firing (the scheduler would otherwise keep
    /// trying to revive an orchestrator in a workspace that's gone). Non-
    /// destructive: the rows remain so the /cron page can show them as orphaned.
    /// Returns rows affected.
    pub async fn disable_cron_jobs_for_workspace(&self, workspace_id: String) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                conn.execute(
                    "UPDATE cron_jobs SET enabled = 0 WHERE workspace_id = ?1 AND enabled = 1",
                    params![workspace_id],
                )
            })
        })
        .await
        .context("spawn_blocking disable_cron_jobs_for_workspace")?
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<AgentRecord>> {
                let mut stmt = conn.prepare(
                    "SELECT id, cli, role, workspace, spawned_at, killed_at, \
                        shim_ready_at, shim_exit_at, shim_exit_code, \
                        workspace_id, spell_run_id, thread_id, last_activity_at, \
                        last_error, last_error_kind, last_error_at \
                 FROM agents \
                 ORDER BY spawned_at ASC",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(AgentRecord {
                        id: row.get(0)?,
                        cli: row.get(1)?,
                        role: row.get(2)?,
                        workspace: row.get(3)?,
                        spawned_at: row.get(4)?,
                        killed_at: row.get(5)?,
                        shim_ready_at: row.get(6)?,
                        shim_exit_at: row.get(7)?,
                        shim_exit_code: row.get(8)?,
                        workspace_id: row.get(9)?,
                        spell_run_id: row.get(10)?,
                        thread_id: row.get(11)?,
                        last_activity_at: row.get(12)?,
                        last_error: row.get(13)?,
                        last_error_kind: row.get(14)?,
                        last_error_at: row.get(15)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking list_agents")?
    }

    // ── messages ─────────────────────────────────────────────────────────

    /// Persist a message with no direction tag (`thread_id = NULL`). Thin
    /// wrapper over [`Self::insert_message_threaded`] — kept so existing
    /// callers/tests that don't care about threads stay unchanged.
    pub async fn insert_message(&self, msg: NewMessage) -> Result<MessageRecord> {
        self.insert_message_threaded(msg, None).await
    }

    /// Persist a message, stamping the direction (`thread_id`) it belongs to.
    /// `Swarm::send_message` is the single choke point that derives this (from
    /// the sender's, else the recipient's, thread) so every message — agent
    /// chatter, user replies, wakes — is self-describing and the UI can hard-
    /// gate a direction's chat instead of guessing from the agent set.
    pub async fn insert_message_threaded(
        &self,
        msg: NewMessage,
        thread_id: Option<String>,
    ) -> Result<MessageRecord> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<MessageRecord> {
            // `meta` is a structured JSON value persisted as TEXT.
            let meta_txt = msg.meta.as_ref().map(|v| v.to_string());
            // Sanitize a dangling `in_reply_to` BEFORE insert. The column is
            // `REFERENCES messages(id)`, so an id that doesn't exist makes the
            // INSERT fail the FK constraint and 500s the whole send — the
            // agent's reply is then lost (live-observed: an opencode worker
            // referencing a stale id got four 500s before a retry landed). An
            // LLM can easily emit a hallucinated / cross-thread / not-yet-
            // committed parent id, so treat a missing parent as "no reply
            // linkage": the message still delivers, just unthreaded.
            let reply_to = match msg.in_reply_to {
                Some(rid) => {
                    let exists: i64 = conn.query_row(
                        "SELECT COUNT(1) FROM messages WHERE id = ?1",
                        params![rid],
                        |r| r.get(0),
                    )?;
                    if exists > 0 { Some(rid) } else { None }
                }
                None => None,
            };
            conn.execute(
                "INSERT INTO messages (from_agent, to_agent, kind, body, sent_at, in_reply_to, thread_id, meta) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    msg.from_agent,
                    msg.to_agent,
                    msg.kind,
                    msg.body,
                    msg.sent_at,
                    reply_to,
                    thread_id,
                    meta_txt
                ],
            )?;
            let id = conn.last_insert_rowid();
            // Clone (don't move) out of the captured `msg`: the retry closure
            // is `FnMut`, so it may run more than once and can't consume it.
            Ok(MessageRecord {
                id,
                from_agent: msg.from_agent.clone(),
                to_agent: msg.to_agent.clone(),
                kind: msg.kind.clone(),
                body: msg.body.clone(),
                sent_at: msg.sent_at,
                delivered_at: None,
                read_at: None,
                in_reply_to: reply_to,
                thread_id: thread_id.clone(),
                meta: msg.meta.clone(),
                thought_trace: None,
            })
        }))
        .await
        .context("spawn_blocking insert_message_threaded")?
    }

    /// The direction (thread) an agent belongs to, by agent id. `Ok(None)` when
    /// the id isn't a known agent or the agent has no thread (main / untagged).
    /// Used to derive a message's `thread_id` from its sender/recipient.
    pub async fn agent_thread_id(&self, agent_id: String) -> Result<Option<String>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<String>> {
                match conn.query_row(
                    "SELECT thread_id FROM agents WHERE id = ?1",
                    params![agent_id],
                    |row| row.get::<_, Option<String>>(0),
                ) {
                    Ok(thread_id) => Ok(thread_id),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e),
                }
            })
        })
        .await
        .context("spawn_blocking agent_thread_id")?
    }

    /// Workspace an agent belongs to, by agent id. `Ok(None)` for unknown or
    /// legacy rows. Thought traces use this for later filtering/reporting.
    pub async fn agent_workspace_id(&self, agent_id: String) -> Result<Option<String>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<String>> {
                match conn.query_row(
                    "SELECT workspace_id FROM agents WHERE id = ?1",
                    params![agent_id],
                    |row| row.get::<_, Option<String>>(0),
                ) {
                    Ok(workspace_id) => Ok(workspace_id),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e),
                }
            })
        })
        .await
        .context("spawn_blocking agent_workspace_id")?
    }

    pub async fn start_thought_trace(
        &self,
        rec: NewThoughtTrace,
        events: Vec<NewThoughtTraceEvent>,
    ) -> Result<ThoughtTraceRecord> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<ThoughtTraceRecord> {
                // One IMMEDIATE transaction so the INSERT + events INSERTs are
                // atomic: a mid-unit BUSY rolls the whole thing back (no orphan
                // 'active' trace that the UNIQUE(trigger_message_id) index would
                // then block forever), restoring with_busy_retry's single-unit
                // retry invariant.
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "INSERT INTO thought_traces \
                     (id, trigger_message_id, agent_id, workspace_id, thread_id, status, \
                      started_at, summary_json, updated_at) \
                     VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7)",
                    params![
                        rec.trigger_message_id,
                        rec.agent_id,
                        rec.workspace_id,
                        rec.thread_id,
                        rec.started_at,
                        rec.summary_json,
                        rec.started_at,
                    ],
                )?;
                let id: String = tx.query_row(
                    "SELECT id FROM thought_traces WHERE rowid = last_insert_rowid()",
                    [],
                    |row| row.get(0),
                )?;
                insert_thought_trace_events(&tx, &id, &events)?;
                let rec = select_thought_trace(&tx, &id)?;
                tx.commit()?;
                Ok(rec)
            })
        })
        .await
        .context("spawn_blocking start_thought_trace")?
    }

    pub async fn complete_latest_thought_trace(
        &self,
        agent_id: String,
        thread_id: Option<String>,
        response_message_id: i64,
        completed_at: i64,
        summary_json: String,
        events: Vec<NewThoughtTraceEvent>,
    ) -> Result<Option<ThoughtTraceRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<Option<ThoughtTraceRecord>> {
                    // IMMEDIATE tx: serialises concurrent append/complete on the
                    // same agent so the summary_json read-modify-write can't lose
                    // a step, and rolls back atomically on a mid-unit BUSY.
                    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    let trace_id = match tx.query_row(
                        "SELECT id FROM thought_traces \
                     WHERE agent_id = ?1 \
                       AND status = 'active' \
                       AND ((thread_id IS NULL AND ?2 IS NULL) OR thread_id = ?2) \
                     ORDER BY started_at DESC LIMIT 1",
                        params![agent_id, thread_id],
                        |row| row.get::<_, String>(0),
                    ) {
                        Ok(id) => Some(id),
                        Err(rusqlite::Error::QueryReturnedNoRows) => match tx.query_row(
                            "SELECT id FROM thought_traces \
                         WHERE agent_id = ?1 AND status = 'active' \
                         ORDER BY started_at DESC LIMIT 1",
                            params![agent_id],
                            |row| row.get::<_, String>(0),
                        ) {
                            Ok(id) => Some(id),
                            Err(rusqlite::Error::QueryReturnedNoRows) => None,
                            Err(e) => return Err(e),
                        },
                        Err(e) => return Err(e),
                    };
                    let Some(trace_id) = trace_id else {
                        return Ok(None);
                    };
                    let existing_summary: String = tx.query_row(
                        "SELECT summary_json FROM thought_traces WHERE id = ?1",
                        params![trace_id],
                        |row| row.get(0),
                    )?;
                    let merged_summary = merge_thought_trace_steps(
                        &existing_summary,
                        parse_thought_trace_steps(&summary_json),
                    );
                    tx.execute(
                        "UPDATE thought_traces \
                     SET response_message_id = ?2, status = 'done', completed_at = ?3, \
                         summary_json = ?4, updated_at = ?3 \
                     WHERE id = ?1",
                        params![trace_id, response_message_id, completed_at, merged_summary],
                    )?;
                    insert_thought_trace_events(&tx, &trace_id, &events)?;
                    let rec = select_thought_trace(&tx, &trace_id)?;
                    tx.commit()?;
                    Ok(Some(rec))
                },
            )
        })
        .await
        .context("spawn_blocking complete_latest_thought_trace")?
    }

    pub async fn append_thought_trace_event(
        &self,
        agent_ids: Vec<String>,
        event: NewThoughtTraceEvent,
    ) -> Result<Option<ThoughtTraceRecord>> {
        if agent_ids.is_empty() {
            return Ok(None);
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<Option<ThoughtTraceRecord>> {
                    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    let placeholders = std::iter::repeat("?")
                        .take(agent_ids.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!(
                        "SELECT id FROM thought_traces \
                         WHERE status = 'active' AND agent_id IN ({placeholders}) \
                         ORDER BY started_at DESC LIMIT 1"
                    );
                    let binds: Vec<rusqlite::types::Value> =
                        agent_ids.iter().map(|id| id.clone().into()).collect();
                    let trace_id = match tx.query_row(
                        &sql,
                        rusqlite::params_from_iter(binds.iter()),
                        |row| row.get::<_, String>(0),
                    ) {
                        Ok(id) => Some(id),
                        Err(rusqlite::Error::QueryReturnedNoRows) => {
                            let recent_placeholders = (3..agent_ids.len() + 3)
                                .map(|idx| format!("?{idx}"))
                                .collect::<Vec<_>>()
                                .join(",");
                            let sql = format!(
                                "SELECT id FROM thought_traces \
                                 WHERE status = 'done' \
                                   AND response_message_id IS NOT NULL \
                                   AND agent_id IN ({recent_placeholders}) \
                                   AND completed_at IS NOT NULL \
                                   AND completed_at <= ?1 \
                                   AND completed_at >= ?2 \
                                 ORDER BY completed_at DESC LIMIT 1"
                            );
                            let mut recent_binds = Vec::with_capacity(agent_ids.len() + 2);
                            recent_binds
                                .push(rusqlite::types::Value::from(event.at));
                            recent_binds.push(rusqlite::types::Value::from(
                                event.at - THOUGHT_TRACE_RECENT_DONE_APPEND_WINDOW_MS,
                            ));
                            recent_binds.extend(
                                agent_ids.iter().cloned().map(rusqlite::types::Value::from),
                            );
                            match tx.query_row(
                                &sql,
                                rusqlite::params_from_iter(recent_binds.iter()),
                                |row| row.get::<_, String>(0),
                            ) {
                                Ok(id) => Some(id),
                                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                                Err(e) => return Err(e),
                            }
                        }
                        Err(e) => return Err(e),
                    };
                    let Some(trace_id) = trace_id else {
                        return Ok(None);
                    };
                    let existing_summary: String = tx.query_row(
                        "SELECT summary_json FROM thought_traces WHERE id = ?1",
                        params![trace_id],
                        |row| row.get(0),
                    )?;
                    let merged_summary = merge_thought_trace_steps(
                        &existing_summary,
                        [event_to_thought_trace_step(&event)],
                    );
                    tx.execute(
                        "UPDATE thought_traces \
                         SET summary_json = ?2, updated_at = ?3 \
                         WHERE id = ?1",
                        params![trace_id, merged_summary, event.at],
                    )?;
                    insert_thought_trace_events(&tx, &trace_id, &[event.clone()])?;
                    let rec = select_thought_trace(&tx, &trace_id)?;
                    tx.commit()?;
                    Ok(Some(rec))
                },
            )
        })
        .await
        .context("spawn_blocking append_thought_trace_event")?
    }

    /// Re-address unread `user → <agent>` messages from a set of (now killed)
    /// agents to a replacement agent. Used when a direction is re-rooted into a
    /// worktree: the orchestrator that read the user's first message is torn
    /// down and respawned with a NEW id, so an as-yet-unanswered user message
    /// would otherwise orphan (the new orchestrator only lists messages
    /// addressed to itself). Only `from_agent='user'` + unread move — agent-to-
    /// agent traffic and already-read history stay put. Returns rows moved.
    pub async fn reassign_unread_user_messages(
        &self,
        old_agents: Vec<String>,
        new_agent: String,
    ) -> Result<usize> {
        if old_agents.is_empty() {
            return Ok(0);
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                let placeholders = std::iter::repeat("?")
                    .take(old_agents.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "UPDATE messages SET to_agent = ? \
                 WHERE from_agent = 'user' AND read_at IS NULL \
                   AND to_agent IN ({placeholders})"
                );
                let mut binds: Vec<rusqlite::types::Value> =
                    Vec::with_capacity(old_agents.len() + 1);
                binds.push(new_agent.clone().into());
                for a in &old_agents {
                    binds.push(a.clone().into());
                }
                conn.execute(&sql, rusqlite::params_from_iter(binds.iter()))
            })
        })
        .await
        .context("spawn_blocking reassign_unread_user_messages")?
    }

    /// Body of the most recent `user → <agent>` message addressed to any of the
    /// given agents (newest by row id). Used on re-root: the first orchestrator
    /// reads the user's opening request to NAME the direction, then is torn down
    /// before writing a ledger — so the replacement orchestrator has neither the
    /// ledger nor the (now-read) message and would re-greet from scratch. We seed
    /// the replacement's `{task}` with this body so its first turn addresses the
    /// real request instead of asking "想干啥?" again. Returns None if no such
    /// message exists (e.g. the direction was named with no user message).
    pub async fn latest_user_message_for_agents(
        &self,
        agents: Vec<String>,
    ) -> Result<Option<String>> {
        if agents.is_empty() {
            return Ok(None);
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<String>> {
                let placeholders = std::iter::repeat("?")
                    .take(agents.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT body FROM messages \
                 WHERE from_agent = 'user' AND to_agent IN ({placeholders}) \
                 ORDER BY id DESC LIMIT 1"
                );
                let binds: Vec<rusqlite::types::Value> =
                    agents.iter().map(|a| a.clone().into()).collect();
                match conn.query_row(&sql, rusqlite::params_from_iter(binds.iter()), |row| {
                    row.get::<_, String>(0)
                }) {
                    Ok(body) => Ok(Some(body)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e),
                }
            })
        })
        .await
        .context("spawn_blocking latest_user_message_for_agents")?
    }

    pub async fn list_messages(&self, opts: ListMessagesOpts) -> Result<Vec<MessageRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<MessageRecord>> {
                // Build WHERE dynamically. We bind via positional params so we
                // can keep the cap on injection surface tight (always-string
                // values, never interpolated).
                let mut wheres: Vec<&str> = Vec::new();
                let mut bound: Vec<rusqlite::types::Value> = Vec::new();
                if let Some(to) = &opts.to_agent {
                    wheres.push("to_agent = ?");
                    bound.push(to.clone().into());
                }
                if let Some(from) = &opts.from_agent {
                    wheres.push("from_agent = ?");
                    bound.push(from.clone().into());
                }
                if let Some(thread) = &opts.thread_id {
                    // Scope to one direction: that thread's rows + legacy/main
                    // null-thread rows (the UI folds those into every view, so
                    // keep them visible rather than orphan old history). P1-04.
                    wheres.push("(m.thread_id = ? OR m.thread_id IS NULL)");
                    bound.push(thread.clone().into());
                }
                if opts.only_undelivered {
                    wheres.push("delivered_at IS NULL");
                }
                let where_sql = if wheres.is_empty() {
                    String::new()
                } else {
                    format!("WHERE {}", wheres.join(" AND "))
                };
                let limit = if opts.limit <= 0 { 200 } else { opts.limit };
                bound.push(limit.into());

                let sql = format!(
                "SELECT m.id, m.from_agent, m.to_agent, m.kind, m.body, m.sent_at, m.delivered_at, \
                        m.read_at, m.in_reply_to, m.thread_id, m.meta, \
                        tt.id, tt.trigger_message_id, tt.response_message_id, tt.agent_id, \
                        tt.workspace_id, tt.thread_id, tt.status, tt.started_at, \
                        tt.completed_at, tt.summary_json, tt.updated_at \
                 FROM messages m \
                 LEFT JOIN thought_traces tt ON tt.response_message_id = m.id \
                 {where_sql} \
                 ORDER BY m.id DESC \
                 LIMIT ?"
            );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(
                    rusqlite::params_from_iter(bound.iter()),
                    message_with_trace_from_row,
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking list_messages")?
    }

    pub async fn search_messages(&self, query: String) -> Result<Vec<MessageRecord>> {
        // CJK queries can never hit the FTS index (see needs_substring_match)
        // — route them to per-token substring LIKEs. The messages table is
        // local-scale, so a full scan is cheap.
        if needs_substring_match(&query) {
            return self.search_messages_substring(query).await;
        }
        // Never feed the raw user string to FTS5 MATCH: bare special characters
        // (`*` `:` `(` `-` `^` `"`, the AND/OR/NOT/NEAR keywords, …) are FTS5
        // *query operators*, so a malformed query would surface as a raw SQLite
        // syntax error (HTTP 500 leaking SQL). We rewrite the input into a set
        // of double-quoted phrase tokens — inside a quoted string every special
        // char is treated as literal token text — joined by spaces (implicit
        // AND). An embedded `"` is escaped per the FTS5 spec by doubling it.
        let match_query = sanitize_fts5_query(&query);
        // If the input contained no tokenizable characters, an empty MATCH is
        // itself a syntax error — just return no hits.
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<MessageRecord>> {
                // Join messages_fts → messages on rowid; order by FTS rank.
                let sql = format!(
                    "SELECT {MESSAGE_SEARCH_COLS} \
                     FROM messages_fts \
                     JOIN messages m ON m.id = messages_fts.rowid \
                     LEFT JOIN thought_traces tt ON tt.response_message_id = m.id \
                     WHERE messages_fts MATCH ?1 \
                     ORDER BY rank \
                     LIMIT 200"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![match_query], message_with_trace_from_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking search_messages")?
    }

    /// Substring fallback for queries `porter unicode61` can't tokenize (CJK —
    /// see [`needs_substring_match`]). One ANDed `LIKE '%token%'` per token
    /// (same implicit-AND semantics as the MATCH token list); `%` `_` `\` in
    /// the input are escaped so user text always matches literally. There is
    /// no FTS rank here, so results come back newest-first.
    async fn search_messages_substring(&self, query: String) -> Result<Vec<MessageRecord>> {
        // Same tokenization rule as sanitize_fts5_query: split on every
        // non-alphanumeric character.
        let tokens: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            // Tokens are alphanumeric-only by construction, so LIKE wildcards
            // (`%`/`_`) can't actually appear — split turned them into
            // separators. Escape anyway in case the split rule is ever
            // loosened (same precedent as the `"` escape in sanitize_fts5_query).
            .map(|t| {
                t.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            })
            .collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<MessageRecord>> {
                let likes = (1..=tokens.len())
                    .map(|i| format!("m.body LIKE ?{i} ESCAPE '\\'"))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let sql = format!(
                    "SELECT {MESSAGE_SEARCH_COLS} \
                     FROM messages m \
                     LEFT JOIN thought_traces tt ON tt.response_message_id = m.id \
                     WHERE {likes} \
                     ORDER BY m.sent_at DESC \
                     LIMIT 200"
                );
                let patterns: Vec<String> = tokens.iter().map(|t| format!("%{t}%")).collect();
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(
                    rusqlite::params_from_iter(patterns.iter()),
                    message_with_trace_from_row,
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking search_messages_substring")?
    }

    pub async fn mark_delivered(&self, ids: Vec<i64>, at_ms: i64) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                // Single statement instead of N (one execute per id). `?` for at_ms
                // then one per id; params bound positionally via params_from_iter.
                let placeholders = vec!["?"; ids.len()].join(",");
                let sql = format!(
                    "UPDATE messages SET delivered_at = ? \
                 WHERE delivered_at IS NULL AND id IN ({placeholders})"
                );
                let mut binds: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len() + 1);
                binds.push(at_ms.into());
                binds.extend(ids.iter().map(|id| (*id).into()));
                conn.execute(&sql, rusqlite::params_from_iter(binds.iter()))?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking mark_delivered")?
    }

    /// Mark messages as read on behalf of `to_agent`. Refuses cross-agent
    /// marks (the WHERE `to_agent = ?` clause) and is idempotent
    /// (`read_at IS NULL`). Returns the ids actually updated this call so
    /// the swarm can broadcast a tight `MessageRead` event.
    pub async fn mark_read(&self, ids: Vec<i64>, to_agent: String, at_ms: i64) -> Result<Vec<i64>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<i64>> {
                // `ids` is caller-supplied and unbounded; SQLite caps a statement at
                // SQLITE_MAX_VARIABLE_NUMBER (999 on the default build). Three fixed
                // params (at_ms×2, to_agent) leave room for ~996 ids, so we chunk at
                // 900/statement to keep a comfortable margin. A flat `IN` over a
                // large batch used to blow the cap and surface as a 500.
                const CHUNK: usize = 900;
                let mut updated: Vec<i64> = Vec::new();
                for chunk in ids.chunks(CHUNK) {
                    // Single UPDATE ... RETURNING per chunk instead of N round-trips.
                    // Also stamp delivered_at when it's still NULL: an MCP read IS the
                    // moment the message reaches the agent's context, so reading is a
                    // valid delivery. This keeps `only_undelivered` self-consistent
                    // (the read path advances the dimension it filters on) and lets
                    // prune_expired reclaim read-but-never-delivered messages, which
                    // its `delivered_at IS NOT NULL AND read_at IS NOT NULL` gate would
                    // otherwise pin forever. COALESCE preserves an earlier send-time
                    // delivery timestamp. Params: read_at, delivered_at, to_agent, ids.
                    let placeholders = vec!["?"; chunk.len()].join(",");
                    let sql = format!(
                        "UPDATE messages SET read_at = ?, delivered_at = COALESCE(delivered_at, ?) \
                     WHERE read_at IS NULL AND to_agent = ? AND id IN ({placeholders}) \
                     RETURNING id"
                    );
                    let mut binds: Vec<rusqlite::types::Value> =
                        Vec::with_capacity(chunk.len() + 3);
                    binds.push(at_ms.into());
                    binds.push(at_ms.into());
                    binds.push(to_agent.clone().into());
                    binds.extend(chunk.iter().map(|id| (*id).into()));
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                        r.get::<_, i64>(0)
                    })?;
                    for id in rows {
                        updated.push(id?);
                    }
                }
                Ok(updated)
            })
        })
        .await
        .context("spawn_blocking mark_read")?
    }

    /// Count messages for `to_agent` that have not yet been read.
    pub async fn count_unread(&self, to_agent: String) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<i64> {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE to_agent = ?1 AND read_at IS NULL",
                    params![to_agent],
                    |row| row.get(0),
                )?;
                Ok(n)
            })
        })
        .await
        .context("spawn_blocking count_unread")?
    }

    /// M6f: atomically find all unread `kind="wake"` messages for `to_agent`,
    /// mark them as read, and return their ids. This is `wake_check`'s
    /// primary signal — it MUST be atomic (single transaction) so a wake
    /// arriving between the SELECT and the UPDATE wouldn't get lost.
    ///
    /// Why this is separate from generic `mark_read`: wake messages are
    /// system triggers, not human-readable mail. `swarm_list_messages`
    /// deliberately skips marking them read (see tools.rs comment), so
    /// nothing else touches them. This method is the ONLY thing that
    /// closes the loop, ensuring each wake fires `wake_check` exactly
    /// once.
    pub async fn consume_wakes(&self, to_agent: String, at_ms: i64) -> Result<Vec<i64>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<i64>> {
                let tx = conn.transaction()?;
                let marked: Vec<i64> = {
                    let mut stmt = tx.prepare(
                        "UPDATE messages SET read_at = ?1 \
                     WHERE to_agent = ?2 AND kind = 'wake' AND read_at IS NULL \
                     RETURNING id",
                    )?;
                    let rows =
                        stmt.query_map(params![at_ms, to_agent], |row| row.get::<_, i64>(0))?;
                    rows.collect::<rusqlite::Result<Vec<i64>>>()?
                };
                tx.commit()?;
                Ok(marked)
            })
        })
        .await
        .context("spawn_blocking consume_wakes")?
    }

    // ── blackboard ───────────────────────────────────────────────────────

    /// Of the given blackboard paths, return the subset that has at least one
    /// op recorded (i.e. the key was written/exists). One query, chunked under
    /// the SQLite variable cap. Used by `list_agents` to detect failed handoffs
    /// (a worker that wrote `<handoff_signal>.error` instead of the success
    /// key) so the DAG can render the node as failed rather than delivered.
    pub async fn blackboard_paths_present(
        &self,
        paths: Vec<String>,
    ) -> Result<std::collections::HashSet<String>> {
        if paths.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<std::collections::HashSet<String>> {
                    const CHUNK: usize = 900;
                    let mut present = std::collections::HashSet::new();
                    for chunk in paths.chunks(CHUNK) {
                        let placeholders = vec!["?"; chunk.len()].join(",");
                        // "Present" = the path's LATEST op is not a delete
                        // tombstone. A bare `SELECT DISTINCT path` would count a
                        // put-then-deleted key as present (its write row lingers
                        // in this append-only ledger forever), so a handoff key
                        // the user deleted via the blackboard panel would keep a
                        // DAG node green / suppress `handoff_missing` even though
                        // the deliverable is gone. Take MAX(id) per path (latest
                        // row) and exclude `op = 'delete'` — same semantics as
                        // `list_blackboard_paths` (routes/swarm.rs) and the
                        // readiness gate, so "does this key exist" has ONE
                        // definition across the codebase. idx_blackboard_path_id
                        // (path, id) serves the grouped MAX(id) lookup.
                        let sql = format!(
                            "SELECT b.path FROM blackboard_ops b \
                             JOIN (SELECT path, MAX(id) AS mid FROM blackboard_ops \
                                   WHERE path IN ({placeholders}) GROUP BY path) m \
                               ON b.id = m.mid \
                             WHERE b.op != 'delete'"
                        );
                        let mut stmt = conn.prepare(&sql)?;
                        let binds: Vec<rusqlite::types::Value> =
                            chunk.iter().map(|p| p.clone().into()).collect();
                        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                            row.get::<_, String>(0)
                        })?;
                        for r in rows {
                            present.insert(r?);
                        }
                    }
                    Ok(present)
                },
            )
        })
        .await
        .context("spawn_blocking blackboard_paths_present")?
    }

    pub async fn insert_blackboard_op(&self, op: NewBlackboardOp) -> Result<BlackboardOpRecord> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<BlackboardOpRecord> {
                conn.execute(
                    "INSERT INTO blackboard_ops (agent_id, op, path, content, sha256, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![op.agent_id, op.op, op.path, op.content, op.sha256, op.at],
                )?;
                let id = conn.last_insert_rowid();
                // Clone (don't move) out of the captured `op`: the retry closure
                // is `FnMut`, so it may run more than once and can't consume it.
                Ok(BlackboardOpRecord {
                    id,
                    agent_id: op.agent_id.clone(),
                    op: op.op.clone(),
                    path: op.path.clone(),
                    content: op.content.clone(),
                    sha256: op.sha256.clone(),
                    at: op.at,
                })
            })
        })
        .await
        .context("spawn_blocking insert_blackboard_op")?
    }

    /// Record a single-file delete as a `delete` tombstone op, mirroring the
    /// `insert_blackboard_op` write pattern so history stays truthful (the past
    /// `write`/`external` rows remain; this just appends the latest fact —
    /// "removed"). Content/sha are empty because a deleted file has none. The
    /// fs removal itself is the route handler's job (it owns the blackboard
    /// root); this is the storage half that keeps the op-log consistent.
    pub async fn record_blackboard_delete(
        &self,
        agent_id: Option<String>,
        path: String,
        at: i64,
    ) -> Result<BlackboardOpRecord> {
        self.insert_blackboard_op(NewBlackboardOp {
            agent_id,
            op: "delete".into(),
            path,
            content: String::new(),
            sha256: String::new(),
            at,
        })
        .await
    }

    /// Returns the latest op for each distinct path. If `path` is `Some`,
    /// only that path's history is returned (most-recent first).
    pub async fn list_blackboard_ops(
        &self,
        path: Option<String>,
    ) -> Result<Vec<BlackboardOpRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<BlackboardOpRecord>> {
                let (sql, bound): (&str, Vec<rusqlite::types::Value>) = match &path {
                    Some(p) => (
                        "SELECT id, agent_id, op, path, content, sha256, at \
                     FROM blackboard_ops WHERE path = ?1 \
                     ORDER BY id DESC LIMIT 200",
                        vec![p.clone().into()],
                    ),
                    None => (
                        // latest per path
                        "SELECT b.id, b.agent_id, b.op, b.path, b.content, b.sha256, b.at \
                     FROM blackboard_ops b \
                     JOIN ( \
                         SELECT path, MAX(id) AS max_id FROM blackboard_ops GROUP BY path \
                     ) latest ON latest.max_id = b.id \
                     ORDER BY b.at DESC LIMIT 200",
                        Vec::new(),
                    ),
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(bound.iter()), |row| {
                    Ok(BlackboardOpRecord {
                        id: row.get(0)?,
                        agent_id: row.get(1)?,
                        op: row.get(2)?,
                        path: row.get(3)?,
                        content: row.get(4)?,
                        sha256: row.get(5)?,
                        at: row.get(6)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking list_blackboard_ops")?
    }

    /// Latest op per path, optionally SCOPED to a key prefix (`<prefix>` itself
    /// or anything under `<prefix>/…`). `scope=None` is the historical global
    /// listing (every path in the store); `scope=Some(p)` restricts to one
    /// direction's `<workspace_id>/<thread_slug>` namespace.
    ///
    /// Why this exists: the blackboard is a single global table with no
    /// workspace/thread column — isolation is by KEY PREFIX only. The default
    /// global list is correct for the collaborative model (workers on one
    /// direction SHARE a prefix and are meant to see each other's keys), but a
    /// fusion competition runs each contestant in its OWN direction and they
    /// must NOT see each other's blackboard. Scoping by the caller's direction
    /// prefix is the single mechanism that serves both: collaborators share a
    /// prefix (still mutually visible), competitors get distinct prefixes
    /// (mutually hidden). GLOB (not LIKE) so a `_` in a slug isn't a wildcard —
    /// mirrors `delete_blackboard_prefix`.
    pub async fn list_blackboard_ops_scoped(
        &self,
        scope: Option<String>,
    ) -> Result<Vec<BlackboardOpRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<BlackboardOpRecord>> {
                let (sql, bound): (&str, Vec<rusqlite::types::Value>) = match &scope {
                    Some(prefix) => (
                        // latest per path, restricted to `<prefix>` or `<prefix>/…`
                        "SELECT b.id, b.agent_id, b.op, b.path, b.content, b.sha256, b.at \
                     FROM blackboard_ops b \
                     JOIN ( \
                         SELECT path, MAX(id) AS max_id FROM blackboard_ops \
                         WHERE path = ?1 OR path GLOB ?2 GROUP BY path \
                     ) latest ON latest.max_id = b.id \
                     ORDER BY b.at DESC LIMIT 200",
                        vec![prefix.clone().into(), format!("{prefix}/*").into()],
                    ),
                    None => (
                        // latest per path, global (historical behaviour)
                        "SELECT b.id, b.agent_id, b.op, b.path, b.content, b.sha256, b.at \
                     FROM blackboard_ops b \
                     JOIN ( \
                         SELECT path, MAX(id) AS max_id FROM blackboard_ops GROUP BY path \
                     ) latest ON latest.max_id = b.id \
                     ORDER BY b.at DESC LIMIT 200",
                        Vec::new(),
                    ),
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(bound.iter()), |row| {
                    Ok(BlackboardOpRecord {
                        id: row.get(0)?,
                        agent_id: row.get(1)?,
                        op: row.get(2)?,
                        path: row.get(3)?,
                        content: row.get(4)?,
                        sha256: row.get(5)?,
                        at: row.get(6)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking list_blackboard_ops_scoped")?
    }

    /// Delete every blackboard op whose path is `prefix` or sits under
    /// `prefix/…`. Used when a direction is deleted to drop its
    /// `<workspace_id>/<thread_slug>/…` ledgers — otherwise the rows orphan
    /// (the slug is gone but its ledgers still show in the blackboard panel).
    /// GLOB (not LIKE) because thread slugs may contain `_`, a LIKE wildcard.
    /// Returns the number of rows removed.
    pub async fn delete_blackboard_prefix(&self, prefix: String) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                // `prefix` itself (a bare key at the dir) + anything beneath it.
                let under = format!("{prefix}/*");
                let n = conn.execute(
                    "DELETE FROM blackboard_ops WHERE path = ?1 OR path GLOB ?2",
                    params![prefix, under],
                )?;
                Ok(n)
            })
        })
        .await
        .context("spawn_blocking delete_blackboard_prefix")?
    }

    // ── pty recordings ───────────────────────────────────────────────────

    pub async fn record_recording_start(&self, rec: NewRecording) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "INSERT INTO pty_recordings (id, agent_id, path, started_at, cols, rows) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        rec.id,
                        rec.agent_id,
                        rec.path,
                        rec.started_at,
                        rec.cols,
                        rec.rows
                    ],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking record_recording_start")?
    }

    pub async fn record_recording_finalize(
        &self,
        id: String,
        finalized_at: i64,
        duration_ms: i64,
        last_seq: i64,
    ) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE pty_recordings \
                 SET finalized_at = ?2, duration_ms = ?3, last_seq = ?4 \
                 WHERE id = ?1 AND finalized_at IS NULL",
                    params![id, finalized_at, duration_ms, last_seq],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking record_recording_finalize")?
    }

    /// Mark agents whose PTY died with the server (rows with NULL
    /// `killed_at`) as killed at `at_ms`. Companion to
    /// `mark_orphan_recordings_finalized`: after a crash restart the
    /// in-memory registry is empty, but `/api/agent` still returns these
    /// rows — without settling them, the UI's reattach reconnects WS to a
    /// non-existent PTY and shows "WS closed (code 1005)" forever.
    pub async fn mark_orphan_agents_killed(&self, at_ms: i64) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                let n = conn.execute(
                    "UPDATE agents SET killed_at = ?1 WHERE killed_at IS NULL",
                    params![at_ms],
                )?;
                Ok(n)
            })
        })
        .await
        .context("spawn_blocking mark_orphan_agents_killed")?
    }

    /// Candidate set for the startup orphan PROCESS reaper
    /// (`swarmx-server/src/reaper.rs`): rows settled as killed at/after
    /// `since_ms` whose shim pid is on file and whose shim never recorded an
    /// exit — i.e. agents the previous server process still had live when it
    /// died, whose real process tree may have been reparented to init instead
    /// of reaped. Callers pass a `since_ms` a small window behind boot: the
    /// window is what keeps an intentionally-killed agent from an OLD session
    /// (whose pid may since have been recycled by an unrelated process) out of
    /// the kill set. `shim_exit_at IS NULL` additionally excludes rows whose
    /// shim was already observed dead — probing those is wasted work (their
    /// pid is gone; a CLI that survived its own dead shim is a known residual
    /// gap, see the reaper). Rows spawned before migration 0029 have
    /// `shim_pid NULL` and are unreachable here — their processes can no
    /// longer be identified, so for them the DB mark is all we can do.
    pub async fn orphaned_agent_processes(
        &self,
        since_ms: i64,
    ) -> Result<Vec<OrphanedAgentProcess>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<OrphanedAgentProcess>> {
                let mut stmt = conn.prepare(
                    "SELECT id, cli, shim_pid, shim_pgid FROM agents \
                     WHERE shim_pid IS NOT NULL \
                       AND shim_exit_at IS NULL \
                       AND killed_at IS NOT NULL AND killed_at >= ?1 \
                     ORDER BY spawned_at ASC",
                )?;
                let rows = stmt.query_map(params![since_ms], |row| {
                    Ok(OrphanedAgentProcess {
                        id: row.get(0)?,
                        cli: row.get(1)?,
                        shim_pid: row.get(2)?,
                        shim_pgid: row.get(3)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking orphaned_agent_processes")?
    }

    /// Finalize any recording rows left in the "live" state — i.e. the
    /// previous server died (crash, SIGKILL, container restart) before its
    /// recorder task could call `record_recording_finalize`. Without this,
    /// the panel shows orphans as "● live" forever.
    ///
    /// We also backfill `duration_ms` from the wall-clock span
    /// (`finalized_at - started_at`). It's not the exact .cast playback
    /// length (the file may be half-flushed), but it's a sane approximation
    /// — far better than leaving it NULL, which rendered every restart-orphan
    /// recording with no duration in the Replays list. `last_seq` stays NULL
    /// (the reattach path treats NULL as "replay from head"). Only fill rows
    /// that don't already have a duration so a genuinely-finalized row isn't
    /// clobbered.
    pub async fn mark_orphan_recordings_finalized(&self, at_ms: i64) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                let n = conn.execute(
                    "UPDATE pty_recordings \
                 SET finalized_at = ?1, \
                     duration_ms = CASE \
                       WHEN duration_ms IS NOT NULL THEN duration_ms \
                       WHEN ?1 > started_at THEN ?1 - started_at \
                       ELSE 0 END \
                 WHERE finalized_at IS NULL",
                    params![at_ms],
                )?;
                Ok(n)
            })
        })
        .await
        .context("spawn_blocking mark_orphan_recordings_finalized")?
    }

    pub async fn list_recordings(&self, agent_id: Option<String>) -> Result<Vec<RecordingRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<RecordingRecord>> {
                let (sql, bound): (&str, Vec<rusqlite::types::Value>) = match &agent_id {
                    Some(a) => (
                        "SELECT id, agent_id, path, started_at, finalized_at, duration_ms, \
                            cols, rows, last_seq \
                     FROM pty_recordings WHERE agent_id = ?1 \
                     ORDER BY started_at DESC LIMIT 200",
                        vec![a.clone().into()],
                    ),
                    None => (
                        "SELECT id, agent_id, path, started_at, finalized_at, duration_ms, \
                            cols, rows, last_seq \
                     FROM pty_recordings \
                     ORDER BY started_at DESC LIMIT 200",
                        Vec::new(),
                    ),
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(bound.iter()), |row| {
                    Ok(RecordingRecord {
                        id: row.get(0)?,
                        agent_id: row.get(1)?,
                        path: row.get(2)?,
                        started_at: row.get(3)?,
                        finalized_at: row.get(4)?,
                        duration_ms: row.get(5)?,
                        cols: row.get(6)?,
                        rows: row.get(7)?,
                        last_seq: row.get(8)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking list_recordings")?
    }

    pub async fn get_recording(&self, id: String) -> Result<Option<RecordingRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<RecordingRecord>> {
                let mut stmt = conn.prepare(
                    "SELECT id, agent_id, path, started_at, finalized_at, duration_ms, \
                        cols, rows, last_seq \
                 FROM pty_recordings WHERE id = ?1",
                )?;
                let mut rows = stmt.query(params![id])?;
                if let Some(row) = rows.next()? {
                    Ok(Some(RecordingRecord {
                        id: row.get(0)?,
                        agent_id: row.get(1)?,
                        path: row.get(2)?,
                        started_at: row.get(3)?,
                        finalized_at: row.get(4)?,
                        duration_ms: row.get(5)?,
                        cols: row.get(6)?,
                        rows: row.get(7)?,
                        last_seq: row.get(8)?,
                    }))
                } else {
                    Ok(None)
                }
            })
        })
        .await
        .context("spawn_blocking get_recording")?
    }

    // ── retention / prune (F5) ───────────────────────────────────────────

    /// Delete rows older than `cutoff_ms` from the three append-only tables,
    /// preserving everything still load-bearing. Returns what was removed.
    ///
    /// Safety constraints (see the audit's F5):
    /// - **blackboard_ops**: only superseded history (`at < cutoff` AND there
    ///   exists a newer row for the same path) is deleted. The latest row per
    ///   path is ALWAYS kept regardless of age — `list_blackboard_ops(None)`
    ///   (discovery) and `reconcile_oplog_from_disk` both read latest-per-path,
    ///   so dropping it would make the key vanish. The FTS index is kept in
    ///   sync by the `blackboard_ad` AFTER DELETE trigger.
    /// - **messages**: only consumed wakes (`kind='wake' AND read_at NOT NULL`)
    ///   and delivered+read normal messages are deleted. Un-consumed wakes
    ///   (`read_at IS NULL`) are NEVER deleted — the WakeCoordinator depends on
    ///   them. Rows still referenced by another message's `in_reply_to` are
    ///   skipped so the delete can't trip the (immediate) FK constraint;
    ///   they age out on a later pass once their children are gone.
    /// - **pty_recordings**: only finalized rows (`finalized_at NOT NULL`) old
    ///   enough are deleted, and their `.cast` file is unlinked best-effort.
    ///   Live recordings are left alone.
    ///
    /// Idempotent and crash-safe: the three deletes run in one transaction,
    /// and a partial run just removes fewer rows next time. After committing
    /// we checkpoint+truncate the WAL and `PRAGMA optimize` (best-effort) so
    /// freed pages don't leave the file bloated.
    pub async fn prune_expired(&self, cutoff_ms: i64) -> Result<PruneStats> {
        let pool = self.pool.clone();
        let (mut stats, files): (PruneStats, Vec<String>) =
            tokio::task::spawn_blocking(move || -> Result<(PruneStats, Vec<String>)> {
                with_busy_retry(
                    &pool,
                    |conn| -> rusqlite::Result<(PruneStats, Vec<String>)> {
                        let tx = conn.transaction()?;

                        // Collect .cast paths BEFORE deleting the rows so we can
                        // unlink them after the tx commits.
                        let files: Vec<String> = {
                            let mut stmt = tx.prepare(
                                "SELECT path FROM pty_recordings \
                             WHERE finalized_at IS NOT NULL AND started_at < ?1",
                            )?;
                            let rows =
                                stmt.query_map(params![cutoff_ms], |r| r.get::<_, String>(0))?;
                            rows.collect::<rusqlite::Result<Vec<_>>>()?
                        };

                        let recordings = tx.execute(
                            "DELETE FROM pty_recordings \
                         WHERE finalized_at IS NOT NULL AND started_at < ?1",
                            params![cutoff_ms],
                        )?;

                        // Superseded blackboard history only — never the latest
                        // row for a path (id < MAX(id) for that path).
                        let blackboard_ops = tx.execute(
                            "DELETE FROM blackboard_ops \
                         WHERE at < ?1 \
                           AND id < (SELECT MAX(id) FROM blackboard_ops b2 \
                                     WHERE b2.path = blackboard_ops.path)",
                            params![cutoff_ms],
                        )?;

                        // Consumed wakes + delivered/read normal messages, and only
                        // ones not referenced as a thread parent (FK-safe).
                        let messages = tx.execute(
                            "DELETE FROM messages \
                         WHERE sent_at < ?1 \
                           AND id NOT IN ( \
                               SELECT in_reply_to FROM messages \
                               WHERE in_reply_to IS NOT NULL) \
                           AND ( \
                               (kind = 'wake' AND read_at IS NOT NULL) \
                            OR (kind != 'wake' AND delivered_at IS NOT NULL \
                                AND read_at IS NOT NULL))",
                            params![cutoff_ms],
                        )?;

                        // Per-step usage + activity logs: append-only and the
                        // highest-frequency writers (one row per agent turn).
                        // Trim rows past the window — aggregate stats within the
                        // window stay intact; only history older than the
                        // retention horizon is dropped.
                        let agent_usage = tx.execute(
                            "DELETE FROM agent_usage WHERE at < ?1",
                            params![cutoff_ms],
                        )?;
                        let agent_activities = tx.execute(
                            "DELETE FROM agent_activities WHERE at < ?1",
                            params![cutoff_ms],
                        )?;

                        tx.commit()?;
                        Ok((
                            PruneStats {
                                blackboard_ops,
                                messages,
                                recordings,
                                recording_files_removed: 0,
                                agent_usage,
                                agent_activities,
                            },
                            files,
                        ))
                    },
                )
            })
            .await
            .context("spawn_blocking prune_expired")??;

        // Unlink pruned .cast files (filesystem, outside the DB tx).
        let mut removed = 0usize;
        for f in &files {
            match std::fs::remove_file(f) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %f, ?e, "prune: failed to unlink .cast file"),
            }
        }
        stats.recording_files_removed = removed;

        // Post-prune hygiene (best-effort): bound the WAL and refresh planner
        // stats. Not part of the prune contract — failures are logged, ignored.
        if !stats.is_empty() {
            let pool2 = self.pool.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(conn) = pool2.get() {
                    if let Err(e) =
                        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;")
                    {
                        tracing::warn!(?e, "prune: post-prune WAL checkpoint/optimize failed");
                    }
                }
            })
            .await;
        }

        Ok(stats)
    }

    // ── workspaces ───────────────────────────────────────────────────────

    /// Insert a workspace. `id` is a hex-encoded random 16-byte value
    /// generated server-side via SQLite's `randomblob` so the storage
    /// crate stays uuid-free. `slug` is the first 8 hex chars of the same
    /// blob — used by the frontend as the URL identifier `/chat/:slug`.
    /// Returns the persisted row (with the generated id / slug / timestamps).
    pub async fn create_workspace(
        &self,
        rec: NewWorkspace,
        created_at: i64,
    ) -> Result<WorkspaceRecord> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<WorkspaceRecord> {
                // INSERT with deterministic generation: lower(hex(randomblob(16)))
                // = 32-char id, substr(...,1,8) = the slug. RETURNING gives us
                // both back so the handler doesn't have to query again.
                //
                // The slug is only 32 bits of entropy (8 hex chars), so a UNIQUE
                // collision, while rare, is non-zero. `with_busy_retry` only retries
                // BUSY/LOCKED, NOT a ConstraintViolation — so without this loop a
                // slug clash would 500 the create-workspace API and the user simply
                // couldn't make a new workspace. Each INSERT regenerates a fresh
                // random slug, so retrying on the (slug) UNIQUE violation almost
                // always succeeds on the next spin.
                const MAX_SLUG_ATTEMPTS: u32 = 5;
                let mut attempt: u32 = 0;
                loop {
                    let result: rusqlite::Result<WorkspaceRecord> = (|| {
                        let mut stmt = conn.prepare(
                            "INSERT INTO workspaces (id, slug, name, cwd, accent, created_at) \
                         VALUES (lower(hex(randomblob(16))), \
                                 substr(lower(hex(randomblob(16))), 1, 8), \
                                 ?1, ?2, ?3, ?4) \
                         RETURNING id, slug, name, cwd, accent, created_at, deleted_at",
                        )?;
                        let mut rows =
                            stmt.query(params![rec.name, rec.cwd, rec.accent, created_at])?;
                        let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                        Ok(WorkspaceRecord {
                            id: row.get(0)?,
                            slug: row.get(1)?,
                            name: row.get(2)?,
                            cwd: row.get(3)?,
                            accent: row.get(4)?,
                            created_at: row.get(5)?,
                            deleted_at: row.get(6)?,
                        })
                    })();
                    match result {
                        Ok(rec) => return Ok(rec),
                        Err(e)
                            if is_constraint_violation(&e) && attempt + 1 < MAX_SLUG_ATTEMPTS =>
                        {
                            attempt += 1;
                            tracing::warn!(attempt, "workspace slug collision; regenerating");
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
        })
        .await
        .context("spawn_blocking create_workspace")?
    }

    /// Return all workspaces, optionally including soft-deleted ones.
    /// Ordered by creation time descending (newest first) so the UI's
    /// left nav puts fresh work at the top.
    pub async fn list_workspaces(&self, include_deleted: bool) -> Result<Vec<WorkspaceRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<WorkspaceRecord>> {
                let sql = if include_deleted {
                    "SELECT id, slug, name, cwd, accent, created_at, deleted_at \
                 FROM workspaces \
                 ORDER BY created_at DESC"
                } else {
                    "SELECT id, slug, name, cwd, accent, created_at, deleted_at \
                 FROM workspaces WHERE deleted_at IS NULL \
                 ORDER BY created_at DESC"
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt.query_map([], |row| {
                    Ok(WorkspaceRecord {
                        id: row.get(0)?,
                        slug: row.get(1)?,
                        name: row.get(2)?,
                        cwd: row.get(3)?,
                        accent: row.get(4)?,
                        created_at: row.get(5)?,
                        deleted_at: row.get(6)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await
        .context("spawn_blocking list_workspaces")?
    }

    /// Look up a single workspace by its primary key. Returns `None` if
    /// not found (including soft-deleted rows — callers that care should
    /// inspect `deleted_at`).
    pub async fn get_workspace_by_id(&self, id: String) -> Result<Option<WorkspaceRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<WorkspaceRecord>> {
                let mut stmt = conn.prepare(
                    "SELECT id, slug, name, cwd, accent, created_at, deleted_at \
                 FROM workspaces WHERE id = ?1",
                )?;
                let mut rows = stmt.query(params![id])?;
                if let Some(row) = rows.next()? {
                    Ok(Some(WorkspaceRecord {
                        id: row.get(0)?,
                        slug: row.get(1)?,
                        name: row.get(2)?,
                        cwd: row.get(3)?,
                        accent: row.get(4)?,
                        created_at: row.get(5)?,
                        deleted_at: row.get(6)?,
                    }))
                } else {
                    Ok(None)
                }
            })
        })
        .await
        .context("spawn_blocking get_workspace_by_id")?
    }

    /// Mark a workspace deleted. Idempotent — re-deleting a row leaves
    /// the existing `deleted_at` untouched. Returns the number of rows
    /// whose `deleted_at` actually transitioned from NULL → set.
    pub async fn soft_delete_workspace(&self, id: String, at_ms: i64) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                let n = conn.execute(
                    "UPDATE workspaces SET deleted_at = ?2 \
                 WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, at_ms],
                )?;
                Ok(n)
            })
        })
        .await
        .context("spawn_blocking soft_delete_workspace")?
    }

    /// Look up the workspace_id of a given agent — the reverse direction
    /// of `agents.workspace_id`. The spell runner uses this to inherit
    /// the caller agent's workspace when MCP `swarm_run_spell` fires.
    /// Returns `None` if the agent isn't found, or if its workspace_id
    /// is NULL (pre-Step-3 rows or legacy `+ Claude` clicks).
    pub async fn get_workspace_id_for_agent(&self, agent_id: String) -> Result<Option<String>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<String>> {
                let mut stmt = conn.prepare("SELECT workspace_id FROM agents WHERE id = ?1")?;
                let mut rows = stmt.query(params![agent_id])?;
                if let Some(row) = rows.next()? {
                    let val: Option<String> = row.get(0)?;
                    Ok(val)
                } else {
                    Ok(None)
                }
            })
        })
        .await
        .context("spawn_blocking get_workspace_id_for_agent")?
    }

    // ── threads (per-workspace directions) ──────────────────────────────

    /// Insert a thread (direction) for a workspace. `id` is generated by the
    /// store (randomblob, uuid-free). `slug` is caller-supplied (meaningful:
    /// URL + blackboard prefix). Retry on the (workspace_id, slug) UNIQUE
    /// collision is the CALLER's job — slug is meaningful, not random.
    pub async fn create_thread(&self, rec: NewThread, created_at: i64) -> Result<ThreadRecord> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<ThreadRecord> {
            let mut stmt = conn.prepare(
                "INSERT INTO threads (id, workspace_id, slug, name, isolation, branch, cwd, state, created_at) \
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 RETURNING id, workspace_id, slug, name, isolation, branch, cwd, state, created_at, deleted_at, model_tier, reasoning_effort",
            )?;
            let mut rows = stmt.query(params![
                rec.workspace_id, rec.slug, rec.name, rec.isolation,
                rec.branch, rec.cwd, rec.state, created_at,
            ])?;
            let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            Ok(ThreadRecord {
                id: row.get(0)?,
                workspace_id: row.get(1)?,
                slug: row.get(2)?,
                name: row.get(3)?,
                isolation: row.get(4)?,
                branch: row.get(5)?,
                cwd: row.get(6)?,
                state: row.get(7)?,
                created_at: row.get(8)?,
                deleted_at: row.get(9)?,
                model_tier: row.get(10)?,
                reasoning_effort: row.get(11)?,
            })
        }))
        .await
        .context("spawn_blocking create_thread")?
    }

    /// List alive threads for a workspace, oldest first (main thread first).
    pub async fn list_threads(&self, workspace_id: String) -> Result<Vec<ThreadRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<ThreadRecord>> {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, slug, name, isolation, branch, cwd, state, created_at, deleted_at, model_tier, reasoning_effort \
                 FROM threads WHERE workspace_id = ?1 AND deleted_at IS NULL \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map(params![workspace_id], |row| {
                Ok(ThreadRecord {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    slug: row.get(2)?,
                    name: row.get(3)?,
                    isolation: row.get(4)?,
                    branch: row.get(5)?,
                    cwd: row.get(6)?,
                    state: row.get(7)?,
                    created_at: row.get(8)?,
                    deleted_at: row.get(9)?,
                    model_tier: row.get(10)?,
                    reasoning_effort: row.get(11)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        }))
        .await
        .context("spawn_blocking list_threads")?
    }

    /// Fetch a single thread by id (alive or deleted — caller checks).
    pub async fn get_thread(&self, id: String) -> Result<Option<ThreadRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<ThreadRecord>> {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, slug, name, isolation, branch, cwd, state, created_at, deleted_at, model_tier, reasoning_effort \
                 FROM threads WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            if let Some(row) = rows.next()? {
                Ok(Some(ThreadRecord {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    slug: row.get(2)?,
                    name: row.get(3)?,
                    isolation: row.get(4)?,
                    branch: row.get(5)?,
                    cwd: row.get(6)?,
                    state: row.get(7)?,
                    created_at: row.get(8)?,
                    deleted_at: row.get(9)?,
                    model_tier: row.get(10)?,
                    reasoning_effort: row.get(11)?,
                }))
            } else {
                Ok(None)
            }
        }))
        .await
        .context("spawn_blocking get_thread")?
    }

    /// Reverse-lookup the thread_id an agent belongs to (for spawn_worker /
    /// swarm_name_thread inheritance). `None` if the agent has no thread (=main)
    /// or isn't found. Mirrors `get_workspace_id_for_agent`.
    pub async fn get_thread_id_for_agent(&self, agent_id: String) -> Result<Option<String>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<Option<String>> {
                let mut stmt = conn.prepare("SELECT thread_id FROM agents WHERE id = ?1")?;
                let mut rows = stmt.query(params![agent_id])?;
                if let Some(row) = rows.next()? {
                    let val: Option<String> = row.get(0)?;
                    Ok(val)
                } else {
                    Ok(None)
                }
            })
        })
        .await
        .context("spawn_blocking get_thread_id_for_agent")?
    }

    /// Update a thread after the AI names it / its worktree is provisioned.
    /// Any `Some(_)` field is written; `None` leaves the column unchanged.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_thread(
        &self,
        id: String,
        name: Option<String>,
        slug: Option<String>,
        isolation: Option<String>,
        branch: Option<String>,
        cwd: Option<String>,
        state: Option<String>,
    ) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    // `deleted_at IS NULL` guard: a background isolation task must
                    // never resurrect a direction that was soft-deleted mid-flight
                    // (matches soft_delete_thread's own guard). On a deleted row
                    // this is a no-op; the caller re-reads to detect that.
                    "UPDATE threads SET \
                    name      = COALESCE(?2, name), \
                    slug      = COALESCE(?3, slug), \
                    isolation = COALESCE(?4, isolation), \
                    branch    = COALESCE(?5, branch), \
                    cwd       = COALESCE(?6, cwd), \
                    state     = COALESCE(?7, state) \
                 WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, name, slug, isolation, branch, cwd, state],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking update_thread")?
    }

    /// Set (or clear) a direction's model override. `None` writes NULL = "use
    /// the global default" — a dedicated setter rather than update_thread's
    /// COALESCE pattern precisely because clearing-to-default must write NULL.
    pub async fn set_thread_model_tier(&self, id: String, tier: Option<String>) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE threads SET model_tier = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, tier],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking set_thread_model_tier")?
    }

    /// Set (or clear) a direction's reasoning/thinking effort. `None` writes
    /// NULL = "the model's own default". Dedicated setter (like model_tier) so
    /// clearing-to-default writes NULL.
    pub async fn set_thread_reasoning_effort(
        &self,
        id: String,
        effort: Option<String>,
    ) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE threads SET reasoning_effort = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, effort],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking set_thread_reasoning_effort")?
    }

    /// Soft-delete a thread (sets deleted_at). Idempotent. Frees its slug for
    /// reuse (the UNIQUE index is alive-only).
    pub async fn soft_delete_thread(&self, id: String, at_ms: i64) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
                conn.execute(
                    "UPDATE threads SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, at_ms],
                )?;
                Ok(())
            })
        })
        .await
        .context("spawn_blocking soft_delete_thread")?
    }

    // ── fusion batches (multi-model competitions) ───────────────────────

    /// Create a fusion batch binding N already-created contestant directions
    /// into one competition. `id` is minted server-side. Returns the row.
    pub async fn create_fusion_batch(
        &self,
        rec: NewFusionBatch,
        created_at: i64,
    ) -> Result<FusionBatchRecord> {
        let pool = self.pool.clone();
        let contestant_json = serde_json::to_string(&rec.contestant_thread_ids)
            .unwrap_or_else(|_| "[]".to_string());
        let check_cmd = rec.check_cmd.clone().filter(|c| !c.trim().is_empty());
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<FusionBatchRecord> {
            let mut stmt = conn.prepare(
                "INSERT INTO fusion_batches \
                   (id, workspace_id, slug, need, contestant_thread_ids_json, judge_thread_id, status, check_cmd, created_at) \
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, NULL, 'running', ?5, ?6) \
                 RETURNING id, workspace_id, slug, need, contestant_thread_ids_json, judge_thread_id, status, check_cmd, created_at, deleted_at",
            )?;
            let mut rows = stmt.query(params![
                rec.workspace_id, rec.slug, rec.need, contestant_json, check_cmd, created_at,
            ])?;
            let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let ids_json: String = row.get(4)?;
            Ok(FusionBatchRecord {
                id: row.get(0)?,
                workspace_id: row.get(1)?,
                slug: row.get(2)?,
                need: row.get(3)?,
                contestant_thread_ids: serde_json::from_str(&ids_json).unwrap_or_default(),
                judge_thread_id: row.get(5)?,
                status: row.get(6)?,
                winner_thread_id: None,
                check_cmd: row.get(7)?,
                created_at: row.get(8)?,
                deleted_at: row.get(9)?,
            })
        }))
        .await
        .context("spawn_blocking create_fusion_batch")?
    }

    /// List alive fusion batches for a workspace, newest first.
    pub async fn list_fusion_batches(&self, workspace_id: String) -> Result<Vec<FusionBatchRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<FusionBatchRecord>> {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, slug, need, contestant_thread_ids_json, judge_thread_id, status, created_at, deleted_at, winner_thread_id, check_cmd \
                 FROM fusion_batches WHERE workspace_id = ?1 AND deleted_at IS NULL \
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![workspace_id], |row| {
                let ids_json: String = row.get(4)?;
                Ok(FusionBatchRecord {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    slug: row.get(2)?,
                    need: row.get(3)?,
                    contestant_thread_ids: serde_json::from_str(&ids_json).unwrap_or_default(),
                    judge_thread_id: row.get(5)?,
                    status: row.get(6)?,
                    winner_thread_id: row.get(9)?,
                    check_cmd: row.get(10)?,
                    created_at: row.get(7)?,
                    deleted_at: row.get(8)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        }))
        .await
        .context("spawn_blocking list_fusion_batches")?
    }

    /// Bind the judge direction to a batch. Does NOT change status — the caller
    /// atomically claims `judging` via `transition_fusion_status('running',
    /// 'judging')` first, so this is a pure judge-thread-id write that can never
    /// resurrect an already-decided batch back into `judging`.
    pub async fn set_fusion_judge(&self, id: String, judge_thread_id: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
            conn.execute(
                "UPDATE fusion_batches SET judge_thread_id = ?2 \
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![id, judge_thread_id],
            )?;
            Ok(())
        }))
        .await
        .context("spawn_blocking set_fusion_judge")?
    }

    /// Every alive batch still in an intermediate stage (`running` | `judging`),
    /// across all workspaces. Used once at startup to recover batches whose
    /// in-memory autochain / judge-watchdog task died with the previous process
    /// (a restart would otherwise leave them spinning forever).
    pub async fn list_active_fusion_batches(&self) -> Result<Vec<FusionBatchRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<Vec<FusionBatchRecord>> {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, slug, need, contestant_thread_ids_json, judge_thread_id, status, created_at, deleted_at, winner_thread_id, check_cmd \
                 FROM fusion_batches WHERE deleted_at IS NULL AND status IN ('running','judging') \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                let ids_json: String = row.get(4)?;
                Ok(FusionBatchRecord {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    slug: row.get(2)?,
                    need: row.get(3)?,
                    contestant_thread_ids: serde_json::from_str(&ids_json).unwrap_or_default(),
                    judge_thread_id: row.get(5)?,
                    status: row.get(6)?,
                    winner_thread_id: row.get(9)?,
                    check_cmd: row.get(10)?,
                    created_at: row.get(7)?,
                    deleted_at: row.get(8)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        }))
        .await
        .context("spawn_blocking list_active_fusion_batches")?
    }

    /// Update a batch's status ('running' | 'judging' | 'done' | 'failed').
    pub async fn set_fusion_status(&self, id: String, status: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
            conn.execute(
                "UPDATE fusion_batches SET status = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                params![id, status],
            )?;
            Ok(())
        }))
        .await
        .context("spawn_blocking set_fusion_status")?
    }

    /// Race-safe status transition: flip to `to` ONLY if the batch is currently
    /// in `from`. Returns rows updated (0 if it already left `from` — e.g. the
    /// judge's own decide flipped it to 'done' between the watchdog's read and
    /// this write, so a stale watchdog can never clobber a real verdict).
    pub async fn transition_fusion_status(
        &self,
        id: String,
        from: String,
        to: String,
    ) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
            let n = conn.execute(
                "UPDATE fusion_batches SET status = ?3 \
                 WHERE id = ?1 AND deleted_at IS NULL AND status = ?2",
                params![id, from, to],
            )?;
            Ok(n)
        }))
        .await
        .context("spawn_blocking transition_fusion_status")?
    }

    /// Claim the winning contestant + flip status to 'done' — a COMPARE-AND-SWAP
    /// on `winner_thread_id IS NULL`, so the FIRST caller to reach it wins and
    /// every later one gets `0`. This is the atomic race gate shared by the two
    /// independent callers of decide: the judge agent's own `curl` and the
    /// server-side judge watchdog (and a double-clicking human). Returns rows
    /// updated: `1` = this caller claimed it, `0` = already decided (or gone).
    /// The handler still validates `winner_thread_id` is one of the batch's
    /// contestants (or the judge thread) before calling — SQLite can't constrain
    /// a value against a JSON array column.
    pub async fn set_fusion_winner(&self, id: String, winner_thread_id: String) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
            let n = conn.execute(
                "UPDATE fusion_batches SET winner_thread_id = ?2, status = 'done' \
                 WHERE id = ?1 AND deleted_at IS NULL AND winner_thread_id IS NULL",
                params![id, winner_thread_id],
            )?;
            Ok(n)
        }))
        .await
        .context("spawn_blocking set_fusion_winner")?
    }

    // ── workspace roots (attached source trees) ─────────────────────────

    /// Attach a dependency-source root folder to a workspace. `id` is minted
    /// server-side via `lower(hex(randomblob(16)))` (same uuid-free trick as
    /// `create_workspace`). Returns the persisted row. The workspace's primary
    /// project dir lives in `workspaces.cwd`; this table only holds the extra
    /// attached roots.
    pub async fn add_workspace_root(
        &self,
        rec: NewWorkspaceRoot,
        created_at: i64,
    ) -> Result<WorkspaceRootRecord> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<WorkspaceRootRecord> {
                let mut stmt = conn.prepare(
                    "INSERT INTO workspace_roots \
                     (id, workspace_id, path, role, label, parent_id, created_at) \
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, ?5, ?6) \
                 RETURNING id, workspace_id, path, role, label, parent_id, created_at",
                )?;
                let mut rows = stmt.query(params![
                    rec.workspace_id,
                    rec.path,
                    rec.role,
                    rec.label,
                    rec.parent_id,
                    created_at,
                ])?;
                let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                Ok(WorkspaceRootRecord {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    path: row.get(2)?,
                    role: row.get(3)?,
                    label: row.get(4)?,
                    parent_id: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
        })
        .await
        .context("spawn_blocking add_workspace_root")?
    }

    /// Return every attached root across all workspaces, ordered by
    /// `created_at` ASC. The list handler groups these by `workspace_id` in a
    /// single pass so it can attach roots to each workspace without N+1.
    pub async fn list_all_workspace_roots(&self) -> Result<Vec<WorkspaceRootRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<Vec<WorkspaceRootRecord>> {
                    let mut stmt = conn.prepare(
                        "SELECT id, workspace_id, path, role, label, parent_id, created_at \
                 FROM workspace_roots \
                 ORDER BY created_at ASC",
                    )?;
                    let rows = stmt.query_map([], |row| {
                        Ok(WorkspaceRootRecord {
                            id: row.get(0)?,
                            workspace_id: row.get(1)?,
                            path: row.get(2)?,
                            role: row.get(3)?,
                            label: row.get(4)?,
                            parent_id: row.get(5)?,
                            created_at: row.get(6)?,
                        })
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                },
            )
        })
        .await
        .context("spawn_blocking list_all_workspace_roots")?
    }

    /// Return the attached roots for a single workspace, ordered by
    /// `created_at` ASC.
    pub async fn list_workspace_roots(
        &self,
        workspace_id: String,
    ) -> Result<Vec<WorkspaceRootRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<Vec<WorkspaceRootRecord>> {
                    let mut stmt = conn.prepare(
                        "SELECT id, workspace_id, path, role, label, parent_id, created_at \
                 FROM workspace_roots WHERE workspace_id = ?1 \
                 ORDER BY created_at ASC",
                    )?;
                    let rows = stmt.query_map(params![workspace_id], |row| {
                        Ok(WorkspaceRootRecord {
                            id: row.get(0)?,
                            workspace_id: row.get(1)?,
                            path: row.get(2)?,
                            role: row.get(3)?,
                            label: row.get(4)?,
                            parent_id: row.get(5)?,
                            created_at: row.get(6)?,
                        })
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                },
            )
        })
        .await
        .context("spawn_blocking list_workspace_roots")?
    }

    /// Look up a single workspace root by its primary key. Returns `None` if
    /// not found. Used to validate a `parent_id` belongs to the same
    /// workspace before attaching a child node under it.
    pub async fn get_workspace_root(&self, id: String) -> Result<Option<WorkspaceRootRecord>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<Option<WorkspaceRootRecord>> {
                    let mut stmt = conn.prepare(
                        "SELECT id, workspace_id, path, role, label, parent_id, created_at \
                 FROM workspace_roots WHERE id = ?1",
                    )?;
                    let mut rows = stmt.query(params![id])?;
                    if let Some(row) = rows.next()? {
                        Ok(Some(WorkspaceRootRecord {
                            id: row.get(0)?,
                            workspace_id: row.get(1)?,
                            path: row.get(2)?,
                            role: row.get(3)?,
                            label: row.get(4)?,
                            parent_id: row.get(5)?,
                            created_at: row.get(6)?,
                        }))
                    } else {
                        Ok(None)
                    }
                },
            )
        })
        .await
        .context("spawn_blocking get_workspace_root")?
    }

    /// Detach a node from a workspace's logical tree by its `id`, CASCADING
    /// to all of its descendants (any row whose `parent_id` chain leads back
    /// to `id`). Returns the total number of rows deleted (0 if nothing
    /// matched the `(workspace_id, id)` pair). The caller refreshes the
    /// workspace's managed context block afterwards.
    ///
    /// SQLite has no recursive cascade on a self-referencing FK, so we
    /// collect the descendant set in memory first (BFS over `parent_id`),
    /// then issue a single bulk `DELETE ... WHERE id IN (...)` — all inside
    /// one `spawn_blocking` on the same connection.
    pub async fn delete_workspace_root(&self, workspace_id: String, id: String) -> Result<usize> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(&pool, |conn| -> rusqlite::Result<usize> {
                // Load every (id, parent_id) edge for this workspace once so we
                // can walk the tree without N round-trips.
                let mut stmt = conn
                    .prepare("SELECT id, parent_id FROM workspace_roots WHERE workspace_id = ?1")?;
                let edges: Vec<(String, Option<String>)> = stmt
                    .query_map(params![workspace_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);

                // The target must exist in this workspace; if not, nothing to do.
                if !edges.iter().any(|(rid, _)| *rid == id) {
                    return Ok(0);
                }

                // BFS: start with {id}; repeatedly pull in rows whose parent_id is
                // already in the collected set, until no new ids appear.
                let mut collected: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                collected.insert(id.clone());
                loop {
                    let mut added = false;
                    for (rid, parent) in &edges {
                        if let Some(p) = parent {
                            if collected.contains(p) && !collected.contains(rid) {
                                collected.insert(rid.clone());
                                added = true;
                            }
                        }
                    }
                    if !added {
                        break;
                    }
                }

                // Single bulk delete scoped to this workspace.
                let ids: Vec<String> = collected.into_iter().collect();
                let placeholders = std::iter::repeat("?")
                    .take(ids.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "DELETE FROM workspace_roots \
                 WHERE workspace_id = ? AND id IN ({placeholders})"
                );
                let mut bound: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len() + 1);
                // Clone (don't move) the captured `workspace_id`: the retry closure
                // is `FnMut` and may run more than once.
                bound.push(workspace_id.clone().into());
                for i in ids {
                    bound.push(i.into());
                }
                let n = conn.execute(&sql, rusqlite::params_from_iter(bound.iter()))?;
                Ok(n)
            })
        })
        .await
        .context("spawn_blocking delete_workspace_root")?
    }

    // ── spell runs ───────────────────────────────────────────────────────

    /// Record a spell run for lineage tracking. Mirrors `create_workspace`
    /// in using SQLite's `randomblob` to mint the id, so this crate
    /// doesn't take a uuid dependency.
    pub async fn create_spell_run(&self, rec: NewSpellRun) -> Result<SpellRunRecord> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<SpellRunRecord> {
            let mut stmt = conn.prepare(
                "INSERT INTO spell_runs (id, workspace_id, spell_name, task, caller_agent_id, started_at) \
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, ?5) \
                 RETURNING id, workspace_id, spell_name, task, caller_agent_id, started_at",
            )?;
            let mut rows = stmt.query(params![
                rec.workspace_id,
                rec.spell_name,
                rec.task,
                rec.caller_agent_id,
                rec.started_at,
            ])?;
            let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            Ok(SpellRunRecord {
                id: row.get(0)?,
                workspace_id: row.get(1)?,
                spell_name: row.get(2)?,
                task: row.get(3)?,
                caller_agent_id: row.get(4)?,
                started_at: row.get(5)?,
            })
        }))
        .await
        .context("spawn_blocking create_spell_run")?
    }

    /// 注册一个 orchestrator 派出来的 ad-hoc worker。注意 agent_id 必须
    /// 先在 `agents` 表存在(`record_agent_spawn` 先跑)— 这里只补 worker
    /// metadata。
    pub async fn record_worker(&self, rec: NewWorker) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || with_busy_retry(&pool, |conn| -> rusqlite::Result<()> {
            // Clone (don't move) the optional columns out of the captured `rec`:
            // the retry closure is `FnMut` and may run more than once.
            let handoff_signal = if rec.handoff_signal.is_empty() {
                None
            } else {
                Some(rec.handoff_signal.clone())
            };
            let depends_on_json = if rec.depends_on_json.is_empty() || rec.depends_on_json == "[]" {
                None
            } else {
                Some(rec.depends_on_json.clone())
            };
            let role_slug = if rec.role_slug.is_empty() {
                None
            } else {
                Some(rec.role_slug.clone())
            };
            let produces_json = if rec.produces_json.is_empty() || rec.produces_json == "[]" {
                None
            } else {
                Some(rec.produces_json.clone())
            };
            let consumes_json = if rec.consumes_json.is_empty() || rec.consumes_json == "[]" {
                None
            } else {
                Some(rec.consumes_json.clone())
            };
            conn.execute(
                "INSERT INTO workers (agent_id, parent_agent_id, role_label, system_prompt, \
                 handoff_signal, depends_on_json, spawned_at, role_slug, produces_json, consumes_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    rec.agent_id,
                    rec.parent_agent_id,
                    rec.role_label,
                    rec.system_prompt,
                    handoff_signal,
                    depends_on_json,
                    rec.spawned_at,
                    role_slug,
                    produces_json,
                    consumes_json,
                ],
            )?;
            Ok(())
        }))
        .await
        .context("spawn_blocking record_worker")?
    }

    /// Batch lookup workers by their agent_id. Returns a map keyed by
    /// agent_id; missing ids are absent from the map. Used by `list_agents`
    /// to derive `parent_agent_id` + `role_label` in one round-trip.
    pub async fn list_workers_by_ids(
        &self,
        ids: Vec<String>,
    ) -> Result<std::collections::HashMap<String, WorkerRecord>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<std::collections::HashMap<String, WorkerRecord>> {
                    let placeholders = std::iter::repeat("?")
                        .take(ids.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!(
                        "SELECT agent_id, parent_agent_id, role_label, system_prompt, \
                                handoff_signal, depends_on_json, spawned_at, \
                                role_slug, produces_json, consumes_json \
                         FROM workers WHERE agent_id IN ({placeholders})"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                        let handoff: Option<String> = row.get(4)?;
                        let deps: Option<String> = row.get(5)?;
                        let role_slug: Option<String> = row.get(7)?;
                        let produces: Option<String> = row.get(8)?;
                        let consumes: Option<String> = row.get(9)?;
                        Ok(WorkerRecord {
                            agent_id: row.get(0)?,
                            parent_agent_id: row.get(1)?,
                            role_label: row.get(2)?,
                            system_prompt: row.get(3)?,
                            handoff_signal: handoff.unwrap_or_default(),
                            depends_on_json: deps.unwrap_or_else(|| "[]".to_string()),
                            spawned_at: row.get(6)?,
                            role_slug: role_slug.unwrap_or_default(),
                            produces_json: produces.unwrap_or_else(|| "[]".to_string()),
                            consumes_json: consumes.unwrap_or_else(|| "[]".to_string()),
                        })
                    })?;
                    let mut out = std::collections::HashMap::with_capacity(ids.len());
                    for rec in rows {
                        let rec = rec?;
                        out.insert(rec.agent_id.clone(), rec);
                    }
                    Ok(out)
                },
            )
        })
        .await
        .context("spawn_blocking list_workers_by_ids")?
    }

    /// Batch lookup spell_runs by id. Returns a map keyed by id; missing ids
    /// are simply absent from the result (callers handle the None case).
    /// Used by `list_agents` to derive each agent's `parent_agent_id` from
    /// its `spell_run.caller_agent_id` in one round-trip instead of N+1.
    pub async fn list_spell_runs_by_ids(
        &self,
        ids: Vec<String>,
    ) -> Result<std::collections::HashMap<String, SpellRunRecord>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            with_busy_retry(
                &pool,
                |conn| -> rusqlite::Result<std::collections::HashMap<String, SpellRunRecord>> {
                    let placeholders = std::iter::repeat("?")
                        .take(ids.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!(
                        "SELECT id, workspace_id, spell_name, task, caller_agent_id, started_at \
                         FROM spell_runs WHERE id IN ({placeholders})"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                        Ok(SpellRunRecord {
                            id: row.get(0)?,
                            workspace_id: row.get(1)?,
                            spell_name: row.get(2)?,
                            task: row.get(3)?,
                            caller_agent_id: row.get(4)?,
                            started_at: row.get(5)?,
                        })
                    })?;
                    let mut out = std::collections::HashMap::with_capacity(ids.len());
                    for rec in rows {
                        let rec = rec?;
                        out.insert(rec.id.clone(), rec);
                    }
                    Ok(out)
                },
            )
        })
        .await
        .context("spawn_blocking list_spell_runs_by_ids")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sanitize_fts5_query_quotes_tokens_and_defuses_operators() {
        // Plain words become quoted phrase tokens (implicit AND).
        assert_eq!(sanitize_fts5_query("planner"), "\"planner\"");
        assert_eq!(sanitize_fts5_query("hello world"), "\"hello\" \"world\"");

        // FTS5 operators / special chars never survive as operators: they are
        // split out as separators, leaving only quoted token text.
        assert_eq!(sanitize_fts5_query("foo AND bar"), "\"foo\" \"AND\" \"bar\"");
        assert_eq!(sanitize_fts5_query("col:val"), "\"col\" \"val\"");
        assert_eq!(sanitize_fts5_query("pre*"), "\"pre\"");
        assert_eq!(sanitize_fts5_query("NEAR(a b)"), "\"NEAR\" \"a\" \"b\"");

        // Inputs that are *only* special characters (the classic crash case,
        // e.g. a lone `"` or `*`) sanitize to the empty string → no results.
        assert_eq!(sanitize_fts5_query("\""), "");
        assert_eq!(sanitize_fts5_query("*"), "");
        assert_eq!(sanitize_fts5_query("\"\"\""), "");
        assert_eq!(sanitize_fts5_query("()-^:"), "");
        assert_eq!(sanitize_fts5_query("   "), "");
        assert_eq!(sanitize_fts5_query(""), "");

        // Non-ASCII token text survives the rewrite (pure-CJK queries no
        // longer reach this function — search_messages routes them to the
        // substring path — but mixed text keeps its non-ASCII tokens).
        assert_eq!(sanitize_fts5_query("会议"), "\"会议\"");
    }

    #[test]
    fn needs_substring_match_flags_cjk_only() {
        assert!(needs_substring_match("共享"));
        assert!(needs_substring_match("review 会议")); // mixed → substring path
        assert!(needs_substring_match("かな"));
        assert!(!needs_substring_match("planner"));
        assert!(!needs_substring_match("café")); // accented Latin tokenizes fine
        assert!(!needs_substring_match(""));
    }

    #[tokio::test]
    async fn search_messages_matches_cjk_substrings() {
        // Regression: `porter unicode61` merges a run of hanzi into ONE FTS
        // token, so `MATCH '"共享"'` silently missed a body like 「…共享区…」
        // and agents concluded "nobody discussed this". The substring path
        // must hit 2-char words too (FTS5 trigram only matches ≥3 chars).
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("swarmx.db")).await.unwrap();
        let msg = |body: &str, at: i64| NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: body.into(),
            sent_at: at,
            in_reply_to: None,
            meta: None,
        };
        store
            .insert_message(msg("把结论写到共享区黑板,别忘了 review 流程", 1))
            .await
            .unwrap();
        store
            .insert_message(msg("pure English message about the planner", 2))
            .await
            .unwrap();
        store
            .insert_message(msg("另一条:会议纪要已经归档", 3))
            .await
            .unwrap();

        // 2-char Chinese word — the case trigram could never match.
        let hits = store.search_messages("共享".into()).await.unwrap();
        assert_eq!(hits.len(), 1, "2-char CJK word must hit, got: {hits:?}");
        assert!(hits[0].body.contains("共享区"));

        // 3-char Chinese word.
        let hits = store.search_messages("会议纪要".into()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("会议纪要"));

        // Multiple CJK tokens AND together, like the MATCH token list.
        let hits = store.search_messages("共享 黑板".into()).await.unwrap();
        assert_eq!(hits.len(), 1);
        let hits = store.search_messages("共享 归档".into()).await.unwrap();
        assert!(hits.is_empty(), "AND semantics: no single body has both");

        // Mixed CJK+Latin query takes the substring path and still hits.
        let hits = store.search_messages("共享 review".into()).await.unwrap();
        assert_eq!(hits.len(), 1);

        // LIKE wildcards can't smuggle in: `%`/`_` are non-alphanumeric, so
        // the tokenizer splits them away — "共_" degenerates to "共" (the same
        // fate FTS specials meet in sanitize_fts5_query).
        let hits = store.search_messages("共_".into()).await.unwrap();
        assert_eq!(hits.len(), 1, "`_` is a separator, not a wildcard");

        // English still goes through FTS (porter stem: planner → planner).
        let hits = store.search_messages("planner".into()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("planner"));
    }

    #[test]
    fn with_suffix_appends_not_replaces() {
        let p = Path::new("/data/swarmx.db");
        assert_eq!(with_suffix(p, "-wal"), PathBuf::from("/data/swarmx.db-wal"));
        assert_eq!(
            with_suffix(p, ".corrupt-9"),
            PathBuf::from("/data/swarmx.db.corrupt-9")
        );
    }

    #[test]
    fn snapshot_creates_usable_backup() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("swarmx.db");
        let mut conn = Connection::open(&path).unwrap();
        crate::schema::run_migrations(&mut conn).unwrap();
        let latest = crate::schema::latest_migration();

        snapshot_before_migration(&conn, &path, latest).unwrap();

        let backup = with_suffix(&path, &format!(".pre-v{latest}.bak"));
        assert!(backup.exists(), "snapshot .bak should exist");
        // The backup is a valid SQLite database that passes its own check.
        let bconn = Connection::open(&backup).unwrap();
        let check: String = bconn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
    }

    #[tokio::test]
    async fn set_fusion_winner_is_compare_and_swap() {
        // The judge agent's curl and the server-side watchdog race to decide the
        // same batch. The CAS guarantees exactly one wins: the first claim flips
        // the batch to done; every later claim is a no-op (0 rows), never an
        // overwrite. This is the race gate the judge watchdog relies on.
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("swarmx.db")).await.unwrap();
        let ws = store
            .create_workspace(
                NewWorkspace { name: "w".into(), cwd: "/tmp".into(), accent: None },
                1,
            )
            .await
            .unwrap();
        let batch = store
            .create_fusion_batch(
                NewFusionBatch {
                    workspace_id: ws.id,
                    slug: "b".into(),
                    need: "n".into(),
                    contestant_thread_ids: vec!["t-a".into(), "t-b".into()],
                    check_cmd: None,
                },
                1,
            )
            .await
            .unwrap();

        // First claim wins; every later claim is a no-op (winner never overwritten).
        assert_eq!(store.set_fusion_winner(batch.id.clone(), "t-a".into()).await.unwrap(), 1);
        assert_eq!(store.set_fusion_winner(batch.id.clone(), "t-b".into()).await.unwrap(), 0);
        let after = store.list_fusion_batches(batch.workspace_id.clone()).await.unwrap();
        let b = after.iter().find(|b| b.id == batch.id).unwrap();
        assert_eq!(b.status, "done");
        assert_eq!(b.winner_thread_id.as_deref(), Some("t-a"));
    }

    #[tokio::test]
    async fn enter_judge_is_a_cas_and_active_batches_are_recoverable() {
        // Entering the judge stage must be an atomic claim: the first
        // running→judging wins, a concurrent second gets 0 (no duplicate judge
        // agent/watchdog), and a decided batch can never be flipped back to
        // judging. set_fusion_judge only records the thread id (no status flip).
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("swarmx.db")).await.unwrap();
        let ws = store
            .create_workspace(NewWorkspace { name: "w".into(), cwd: "/tmp".into(), accent: None }, 1)
            .await
            .unwrap();
        let mk = |slug: &str| NewFusionBatch {
            workspace_id: ws.id.clone(),
            slug: slug.into(),
            need: "n".into(),
            contestant_thread_ids: vec!["t-a".into()],
            check_cmd: None,
        };
        let running = store.create_fusion_batch(mk("run"), 1).await.unwrap();
        let judging = store.create_fusion_batch(mk("judge"), 2).await.unwrap();
        let done = store.create_fusion_batch(mk("done"), 3).await.unwrap();

        // CAS claim: first wins, second is a no-op.
        assert_eq!(
            store.transition_fusion_status(judging.id.clone(), "running".into(), "judging".into()).await.unwrap(),
            1
        );
        assert_eq!(
            store.transition_fusion_status(judging.id.clone(), "running".into(), "judging".into()).await.unwrap(),
            0,
            "second claim must not re-flip"
        );
        // set_fusion_judge records the thread id WITHOUT touching status.
        store.set_fusion_judge(judging.id.clone(), "t-judge".into()).await.unwrap();

        // A decided batch cannot be dragged back into judging.
        assert_eq!(store.set_fusion_winner(done.id.clone(), "t-a".into()).await.unwrap(), 1);
        assert_eq!(
            store.transition_fusion_status(done.id.clone(), "running".into(), "judging".into()).await.unwrap(),
            0,
            "done batch must not re-enter judging"
        );

        // Recovery sweep sees exactly the intermediate batches (running + judging),
        // never the decided one.
        let active = store.list_active_fusion_batches().await.unwrap();
        let ids: std::collections::HashSet<_> = active.iter().map(|b| b.id.clone()).collect();
        assert!(ids.contains(&running.id));
        assert!(ids.contains(&judging.id));
        assert!(!ids.contains(&done.id));
        let judged = active.iter().find(|b| b.id == judging.id).unwrap();
        assert_eq!(judged.status, "judging");
        assert_eq!(judged.judge_thread_id.as_deref(), Some("t-judge"));
    }

    #[test]
    fn prune_keeps_only_newest_snapshots() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("swarmx.db");
        for v in 1..=5 {
            std::fs::write(with_suffix(&path, &format!(".pre-v{v}.bak")), b"x").unwrap();
        }
        prune_old_snapshots(&path, 3);
        let remaining = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".pre-v"))
            .count();
        assert_eq!(remaining, 3, "should keep only the 3 newest snapshots");
    }
}
