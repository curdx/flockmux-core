//! Migration runner. Migrations live in `MIGRATIONS` (version, sql) and are
//! applied in ascending order; each runs inside `BEGIN IMMEDIATE` so the write
//! lock is taken up front — `busy_timeout` applies at BEGIN — instead of a
//! DEFERRED transaction failing mid-batch on a WAL lock upgrade
//! (SQLITE_BUSY_SNAPSHOT cannot be retried without rolling back). The
//! transaction itself is what keeps a concurrent reader from ever observing a
//! half-applied schema (SQLite WAL allows readers during writes, and our
//! triggers + FTS5 are not atomic-without-tx).
//!
//! `run_migrations` refuses to start when the database's recorded version is
//! HIGHER than the newest migration this binary knows about: an older binary
//! must never write to a database a newer binary already upgraded, or it would
//! silently corrupt rows under a schema it does not understand. Adding a
//! migration is a one-line edit to `MIGRATIONS`; `latest_migration()` and the
//! upper-bound guard derive from that list automatically.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

const MIGRATION_0001: &str = include_str!("../migrations/0001_init.sql");
const MIGRATION_0002: &str = include_str!("../migrations/0002_pty_recordings.sql");
const MIGRATION_0003: &str = include_str!("../migrations/0003_in_reply_to.sql");
const MIGRATION_0004: &str = include_str!("../migrations/0004_workspaces.sql");
const MIGRATION_0005: &str = include_str!("../migrations/0005_workers.sql");
const MIGRATION_0006: &str = include_str!("../migrations/0006_workspace_roots.sql");
const MIGRATION_0007: &str = include_str!("../migrations/0007_workspace_root_parent.sql");
const MIGRATION_0008: &str = include_str!("../migrations/0008_blackboard_path_id.sql");
const MIGRATION_0009: &str = include_str!("../migrations/0009_threads.sql");
const MIGRATION_0010: &str = include_str!("../migrations/0010_message_thread_id.sql");
const MIGRATION_0011: &str = include_str!("../migrations/0011_worker_role_typed_handoff.sql");
const MIGRATION_0012: &str = include_str!("../migrations/0012_message_meta.sql");
const MIGRATION_0013: &str = include_str!("../migrations/0013_agent_last_activity.sql");
const MIGRATION_0014: &str = include_str!("../migrations/0014_thread_model_tier.sql");
const MIGRATION_0015: &str = include_str!("../migrations/0015_thread_reasoning_effort.sql");
const MIGRATION_0016: &str = include_str!("../migrations/0016_agent_usage.sql");
const MIGRATION_0017: &str = include_str!("../migrations/0017_worker_task_status.sql");
const MIGRATION_0018: &str = include_str!("../migrations/0018_cron_jobs.sql");
const MIGRATION_0019: &str = include_str!("../migrations/0019_goals.sql");
const MIGRATION_0020: &str = include_str!("../migrations/0020_goal_evidence.sql");
const MIGRATION_0021: &str = include_str!("../migrations/0021_thought_traces.sql");
const MIGRATION_0022: &str = include_str!("../migrations/0022_agent_last_error.sql");
const MIGRATION_0023: &str = include_str!("../migrations/0023_agent_activities.sql");
const MIGRATION_0024: &str = include_str!("../migrations/0024_cron_tz_offset.sql");
const MIGRATION_0025: &str =
    include_str!("../migrations/0025_message_perf_indexes.sql");
const MIGRATION_0026: &str = include_str!("../migrations/0026_fusion_batches.sql");
const MIGRATION_0027: &str = include_str!("../migrations/0027_fusion_winner.sql");
const MIGRATION_0028: &str = include_str!("../migrations/0028_fusion_check_cmd.sql");
const MIGRATION_0029: &str = include_str!("../migrations/0029_agent_shim_pid.sql");
const MIGRATION_0030: &str = include_str!("../migrations/0030_workspace_budget.sql");

/// Every migration in apply order. Append new entries here — nothing else needs
/// to change; `latest_migration()` and the upper-bound guard derive from this.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, MIGRATION_0001),
    (2, MIGRATION_0002),
    (3, MIGRATION_0003),
    (4, MIGRATION_0004),
    (5, MIGRATION_0005),
    (6, MIGRATION_0006),
    (7, MIGRATION_0007),
    (8, MIGRATION_0008),
    (9, MIGRATION_0009),
    (10, MIGRATION_0010),
    (11, MIGRATION_0011),
    (12, MIGRATION_0012),
    (13, MIGRATION_0013),
    (14, MIGRATION_0014),
    (15, MIGRATION_0015),
    (16, MIGRATION_0016),
    (17, MIGRATION_0017),
    (18, MIGRATION_0018),
    (19, MIGRATION_0019),
    (20, MIGRATION_0020),
    (21, MIGRATION_0021),
    (22, MIGRATION_0022),
    (23, MIGRATION_0023),
    (24, MIGRATION_0024),
    (25, MIGRATION_0025),
    (26, MIGRATION_0026),
    (27, MIGRATION_0027),
    (28, MIGRATION_0028),
    (29, MIGRATION_0029),
    (30, MIGRATION_0030),
];

/// Highest migration version this binary can apply.
pub(crate) fn latest_migration() -> i64 {
    MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0)
}

pub(crate) fn run_migrations(conn: &mut Connection) -> Result<()> {
    let current = current_version(conn).unwrap_or(0);
    let latest = latest_migration();

    // Upper-bound guard: a database upgraded by a NEWER swarmx must not be
    // written by this (older) binary — it would apply no migrations yet keep
    // writing under a schema it doesn't understand, silently corrupting data.
    if current > latest {
        bail!(
            "数据库 schema 版本 v{current} 高于本 swarmx 版本支持的 v{latest}；\
             为避免旧版本以不兼容的 schema 写坏数据，已拒绝启动。请升级 swarmx 后重试。"
        );
    }

    tracing::debug!(current, latest, "running swarmx-storage migrations");
    for (version, sql) in MIGRATIONS {
        if current < *version {
            apply(conn, *version, sql).with_context(|| format!("apply migration {version}"))?;
        }
    }
    Ok(())
}

pub(crate) fn current_version(conn: &Connection) -> Result<i64> {
    // schema_version may not exist yet (fresh DB).
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    let v: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    Ok(v)
}

fn apply(conn: &mut Connection, version: i64, sql: &str) -> Result<()> {
    // BEGIN IMMEDIATE (not the DEFERRED default): take the write lock before
    // running any statement so a competing writer fails us at BEGIN — where
    // busy_timeout still applies — not mid-batch. Same behavior store.rs uses
    // for its multi-write transactions.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // execute_batch handles the multi-statement migration in one call.
    tx.execute_batch(sql)
        .with_context(|| format!("execute migration {version}"))?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_migrates_to_latest() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), latest_migration());
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        // 二次运行不应报错，也不应重复应用（版本保持不变）。
        run_migrations(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), latest_migration());
    }

    #[test]
    fn rejects_database_newer_than_binary() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        // 伪造一个比本二进制更高的版本，模拟旧二进制打开新库。
        let ahead = latest_migration() + 1;
        conn.execute("INSERT INTO schema_version (version) VALUES (?1)", [ahead])
            .unwrap();
        let err = run_migrations(&mut conn).unwrap_err();
        assert!(
            err.to_string().contains("高于本 swarmx"),
            "expected upper-bound guard to trip, got: {err}"
        );
    }
}
