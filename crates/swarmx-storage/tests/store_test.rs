//! Integration tests for `swarmx_storage::Store`. Each test owns its own
//! `TempDir` so they parallelise safely.

use swarmx_storage::{
    ListMessagesOpts, MessageRecord, NewAgent, NewBlackboardOp, NewFusionBatch, NewGoal,
    NewGoalEvidence,
    NewMessage, NewRecording, NewThoughtTrace, NewThoughtTraceEvent, NewThread, NewWorker,
    NewWorkspace, Store, ThoughtTraceStep,
};
use tempfile::TempDir;

async fn fresh_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("swarmx.db");
    let store = Store::open(&path).await.expect("open store");
    (dir, store)
}

fn ts(base: i64) -> i64 {
    1_700_000_000_000 + base
}

// ── corruption recovery (P0-1) ────────────────────────────────────────────

#[tokio::test]
async fn corrupt_database_is_archived_and_rebuilt() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("swarmx.db");
    // Non-SQLite bytes → `PRAGMA quick_check` / open fails (NOTADB).
    std::fs::write(&path, b"this is definitely not a valid sqlite database file").unwrap();

    // open must RECOVER (archive + rebuild), not panic or bubble an error.
    let store = Store::open(&path)
        .await
        .expect("open should recover from a corrupt db instead of crashing");

    // The corrupt file is moved aside to a `*.corrupt-*` archive.
    let archived = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
    assert!(archived, "corrupt database should be archived aside");

    // The freshly rebuilt database is usable: a write goes through.
    store
        .insert_message_threaded(
            NewMessage {
                from_agent: "user".into(),
                to_agent: "agent-a".into(),
                kind: "task".into(),
                body: "after rebuild".into(),
                sent_at: ts(1),
                in_reply_to: None,
                meta: None,
            },
            Some("t-1".into()),
        )
        .await
        .expect("rebuilt database should accept writes");
}

// ── retention: usage/activity tables are bounded (P1-9) ───────────────────

#[tokio::test]
async fn prune_trims_old_agent_usage_keeps_recent() {
    let (_dir, store) = fresh_store().await;
    let cutoff = ts(1000);
    // One row older than the window, one inside it.
    store
        .insert_agent_usage("claude-aaaa".into(), Some("sonnet".into()), 10, 20, 0, 0, ts(500))
        .await
        .unwrap();
    store
        .insert_agent_usage("claude-aaaa".into(), Some("sonnet".into()), 1, 2, 0, 0, ts(2000))
        .await
        .unwrap();

    let stats = store.prune_expired(cutoff).await.unwrap();
    assert_eq!(stats.agent_usage, 1, "the row older than cutoff is pruned");

    // The recent row stays (in-window usage stats remain queryable); a second
    // pass removes nothing more. agent_activities uses the identical
    // `DELETE WHERE at < cutoff` path in the same transaction.
    let again = store.prune_expired(cutoff).await.unwrap();
    assert_eq!(again.agent_usage, 0);
}

// ── thought traces ───────────────────────────────────────────────────────

#[tokio::test]
async fn thought_trace_roundtrips_on_response_message() {
    let (_dir, store) = fresh_store().await;
    let user = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "user".into(),
                to_agent: "agent-a".into(),
                kind: "task".into(),
                body: "build it".into(),
                sent_at: ts(1),
                in_reply_to: None,
                meta: None,
            },
            Some("thread-1".into()),
        )
        .await
        .unwrap();
    let start_steps = vec![ThoughtTraceStep {
        phase: "understand".into(),
        label: "理解用户请求".into(),
        source: "system".into(),
        at: ts(1),
    }];
    let trace = store
        .start_thought_trace(
            NewThoughtTrace {
                trigger_message_id: user.id,
                agent_id: "agent-a".into(),
                workspace_id: Some("ws-1".into()),
                thread_id: Some("thread-1".into()),
                started_at: ts(1),
                summary_json: serde_json::to_string(&start_steps).unwrap(),
            },
            vec![NewThoughtTraceEvent {
                phase: "understand".into(),
                label: "理解用户请求".into(),
                source: "system".into(),
                at: ts(1),
                meta: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(trace.status, "active");

    let reply = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "agent-a".into(),
                to_agent: "user".into(),
                kind: "reply".into(),
                body: "done".into(),
                sent_at: ts(5),
                in_reply_to: Some(user.id),
                meta: None,
            },
            Some("thread-1".into()),
        )
        .await
        .unwrap();
    let done_steps = vec![
        start_steps[0].clone(),
        ThoughtTraceStep {
            phase: "answer".into(),
            label: "整理结果并回复用户".into(),
            source: "system".into(),
            at: ts(5),
        },
    ];
    let completed = store
        .complete_latest_thought_trace(
            "agent-a".into(),
            Some("thread-1".into()),
            reply.id,
            ts(5),
            serde_json::to_string(&done_steps).unwrap(),
            vec![NewThoughtTraceEvent {
                phase: "answer".into(),
                label: "整理结果并回复用户".into(),
                source: "system".into(),
                at: ts(5),
                meta: None,
            }],
        )
        .await
        .unwrap()
        .expect("trace completed");
    assert_eq!(completed.status, "done");
    assert_eq!(completed.response_message_id, Some(reply.id));

    let rows = store
        .list_messages(ListMessagesOpts {
            to_agent: None,
            from_agent: None,
            thread_id: None,
            only_undelivered: false,
            limit: 10,
        })
        .await
        .unwrap();
    let persisted = rows
        .into_iter()
        .find(|m| m.id == reply.id)
        .and_then(|m| m.thought_trace)
        .expect("reply has trace");
    assert_eq!(persisted.id, completed.id);
    assert_eq!(persisted.completed_at, Some(ts(5)));
}

#[tokio::test]
async fn thought_trace_appends_activity_and_preserves_it_on_complete() {
    let (_dir, store) = fresh_store().await;
    let user = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "user".into(),
                to_agent: "orch".into(),
                kind: "task".into(),
                body: "ship it".into(),
                sent_at: ts(1),
                in_reply_to: None,
                meta: None,
            },
            Some("thread-1".into()),
        )
        .await
        .unwrap();
    let start_steps = vec![ThoughtTraceStep {
        phase: "understand".into(),
        label: "理解用户请求".into(),
        source: "system".into(),
        at: ts(1),
    }];
    store
        .start_thought_trace(
            NewThoughtTrace {
                trigger_message_id: user.id,
                agent_id: "orch".into(),
                workspace_id: Some("ws-1".into()),
                thread_id: Some("thread-1".into()),
                started_at: ts(1),
                summary_json: serde_json::to_string(&start_steps).unwrap(),
            },
            vec![],
        )
        .await
        .unwrap();
    store
        .append_thought_trace_event(
            vec!["worker-a".into(), "orch".into()],
            NewThoughtTraceEvent {
                phase: "tool_ok".into(),
                label: "完成工具: Edit src/app.tsx".into(),
                source: "agent".into(),
                at: ts(3),
                meta: None,
            },
        )
        .await
        .unwrap()
        .expect("active trace matched parent candidate");

    let reply = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "orch".into(),
                to_agent: "user".into(),
                kind: "reply".into(),
                body: "done".into(),
                sent_at: ts(5),
                in_reply_to: Some(user.id),
                meta: None,
            },
            Some("thread-1".into()),
        )
        .await
        .unwrap();
    let completed = store
        .complete_latest_thought_trace(
            "orch".into(),
            Some("thread-1".into()),
            reply.id,
            ts(5),
            serde_json::to_string(&[ThoughtTraceStep {
                phase: "answer".into(),
                label: "整理结果并回复用户".into(),
                source: "system".into(),
                at: ts(5),
            }])
            .unwrap(),
            vec![],
        )
        .await
        .unwrap()
        .expect("completed");
    let labels = serde_json::from_str::<Vec<ThoughtTraceStep>>(&completed.summary_json)
        .unwrap()
        .into_iter()
        .map(|s| s.label)
        .collect::<Vec<_>>();
    assert!(labels.contains(&"理解用户请求".to_string()));
    assert!(labels.contains(&"完成工具: Edit src/app.tsx".to_string()));
    assert!(labels.contains(&"整理结果并回复用户".to_string()));
}

#[tokio::test]
async fn thought_trace_appends_late_activity_to_recently_completed_trace() {
    let (_dir, store) = fresh_store().await;
    let user = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "user".into(),
                to_agent: "orch".into(),
                kind: "task".into(),
                body: "reply please".into(),
                sent_at: ts(1),
                in_reply_to: None,
                meta: None,
            },
            Some("thread-1".into()),
        )
        .await
        .unwrap();
    store
        .start_thought_trace(
            NewThoughtTrace {
                trigger_message_id: user.id,
                agent_id: "orch".into(),
                workspace_id: Some("ws-1".into()),
                thread_id: Some("thread-1".into()),
                started_at: ts(1),
                summary_json: serde_json::to_string(&[ThoughtTraceStep {
                    phase: "understand".into(),
                    label: "理解用户请求".into(),
                    source: "system".into(),
                    at: ts(1),
                }])
                .unwrap(),
            },
            vec![],
        )
        .await
        .unwrap();

    let reply = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "orch".into(),
                to_agent: "user".into(),
                kind: "reply".into(),
                body: "done".into(),
                sent_at: ts(5),
                in_reply_to: Some(user.id),
                meta: None,
            },
            Some("thread-1".into()),
        )
        .await
        .unwrap();
    store
        .complete_latest_thought_trace(
            "orch".into(),
            Some("thread-1".into()),
            reply.id,
            ts(5),
            serde_json::to_string(&[ThoughtTraceStep {
                phase: "answer".into(),
                label: "整理结果并回复用户".into(),
                source: "system".into(),
                at: ts(5),
            }])
            .unwrap(),
            vec![],
        )
        .await
        .unwrap()
        .expect("completed");

    let updated = store
        .append_thought_trace_event(
            vec!["orch".into()],
            NewThoughtTraceEvent {
                phase: "tool_ok".into(),
                label: "完成工具: swarm_send_message".into(),
                source: "agent".into(),
                at: ts(6),
                meta: None,
            },
        )
        .await
        .unwrap()
        .expect("recent completed trace matched");

    let labels = serde_json::from_str::<Vec<ThoughtTraceStep>>(&updated.summary_json)
        .unwrap()
        .into_iter()
        .map(|s| s.label)
        .collect::<Vec<_>>();
    assert!(labels.contains(&"整理结果并回复用户".to_string()));
    assert!(labels.contains(&"完成工具: swarm_send_message".to_string()));
}

// ── goals ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn goals_roundtrip_and_status_update() {
    let (_dir, store) = fresh_store().await;
    let ws = store
        .create_workspace(
            NewWorkspace {
                name: "proj".into(),
                cwd: "/tmp/proj".into(),
                accent: None,
            },
            ts(0),
        )
        .await
        .unwrap();
    let thread = store
        .create_thread(
            NewThread {
                workspace_id: ws.id.clone(),
                slug: "main".into(),
                name: Some("main".into()),
                isolation: "shared".into(),
                branch: None,
                cwd: ws.cwd.clone(),
                state: "ready".into(),
            },
            ts(1),
        )
        .await
        .unwrap();

    store
        .upsert_goal(NewGoal {
            id: "g-main".into(),
            workspace_id: ws.id.clone(),
            thread_id: None,
            objective: "Ship billing guardrails".into(),
            success_criteria: "[\"Claude stays interactive\"]".into(),
            status: "active".into(),
            budget_tokens: Some(10_000),
            created_at: ts(2),
            updated_at: ts(2),
            completed_at: None,
        })
        .await
        .unwrap();
    store
        .upsert_goal(NewGoal {
            id: "g-thread".into(),
            workspace_id: ws.id.clone(),
            thread_id: Some(thread.id.clone()),
            objective: "Wire goals API".into(),
            success_criteria: "[]".into(),
            status: "active".into(),
            budget_tokens: None,
            created_at: ts(3),
            updated_at: ts(3),
            completed_at: None,
        })
        .await
        .unwrap();

    let all = store.list_goals(Some(ws.id.clone()), None).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, "g-thread", "newest goal first");

    let main = store
        .list_goals(Some(ws.id.clone()), Some(None))
        .await
        .unwrap();
    assert_eq!(main.len(), 1);
    assert_eq!(main[0].id, "g-main");

    let scoped = store
        .list_goals(Some(ws.id.clone()), Some(Some(thread.id.clone())))
        .await
        .unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].id, "g-thread");

    let changed = store
        .update_goal_status("g-thread".into(), "complete".into(), ts(4), Some(ts(4)))
        .await
        .unwrap();
    assert!(changed);
    let scoped = store
        .list_goals(Some(ws.id), Some(Some(thread.id)))
        .await
        .unwrap();
    assert_eq!(scoped[0].status, "complete");
    assert_eq!(scoped[0].completed_at, Some(ts(4)));

    let missing = store
        .update_goal_status("missing".into(), "blocked".into(), ts(5), None)
        .await
        .unwrap();
    assert!(!missing);

    store
        .add_goal_evidence(NewGoalEvidence {
            id: "ev-1".into(),
            goal_id: "g-thread".into(),
            kind: "test".into(),
            summary: "cargo test passed".into(),
            source_agent_id: Some("agent-1".into()),
            blackboard_path: Some("ws/main/test.done".into()),
            command: Some("cargo test".into()),
            created_at: ts(6),
        })
        .await
        .unwrap();
    store
        .add_goal_evidence(NewGoalEvidence {
            id: "ev-2".into(),
            goal_id: "g-thread".into(),
            kind: "note".into(),
            summary: "review complete".into(),
            source_agent_id: None,
            blackboard_path: None,
            command: None,
            created_at: ts(7),
        })
        .await
        .unwrap();
    let evidence = store
        .list_goal_evidence("g-thread".into(), 10)
        .await
        .unwrap();
    assert_eq!(evidence.len(), 2);
    assert_eq!(evidence[0].id, "ev-2", "newest evidence first");
    assert_eq!(evidence[1].command.as_deref(), Some("cargo test"));
}

// ── agents ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn schema_bootstraps_on_open() {
    let (_dir, store) = fresh_store().await;
    let agents = store.list_agents().await.unwrap();
    assert!(agents.is_empty());
}

#[tokio::test]
async fn agent_spawn_then_list() {
    let (_dir, store) = fresh_store().await;
    store
        .record_agent_spawn(NewAgent {
            id: "a-1".into(),
            cli: "claude".into(),
            role: "explorer".into(),
            workspace: "/tmp/a".into(),
            spawned_at: ts(0),
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();
    let agents = store.list_agents().await.unwrap();
    assert_eq!(agents.len(), 1);
    let a = &agents[0];
    assert_eq!(a.id, "a-1");
    assert_eq!(a.cli, "claude");
    assert!(a.killed_at.is_none());
    assert!(a.shim_ready_at.is_none());
}

#[tokio::test]
async fn agent_lifecycle_updates_idempotent() {
    let (_dir, store) = fresh_store().await;
    store
        .record_agent_spawn(NewAgent {
            id: "a-2".into(),
            cli: "codex".into(),
            role: "codex".into(),
            workspace: "/tmp/b".into(),
            spawned_at: ts(0),
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();
    store.record_shim_ready("a-2".into(), ts(10)).await.unwrap();
    // Second call must be a no-op (idempotent — first non-NULL wins).
    store.record_shim_ready("a-2".into(), ts(99)).await.unwrap();
    store
        .record_shim_exit("a-2".into(), 0, ts(20))
        .await
        .unwrap();
    store.record_agent_kill("a-2".into(), ts(30)).await.unwrap();

    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert_eq!(a.shim_ready_at, Some(ts(10)), "ready timestamp pinned");
    assert_eq!(a.shim_exit_code, Some(0));
    assert_eq!(a.killed_at, Some(ts(30)));
}

#[tokio::test]
async fn touch_agent_activity_advances_monotonically() {
    let (_dir, store) = fresh_store().await;
    store
        .record_agent_spawn(NewAgent {
            id: "a-3".into(),
            cli: "claude".into(),
            role: "backend".into(),
            workspace: "/tmp/c".into(),
            spawned_at: ts(0),
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();

    // Fresh row: no activity yet (F3 stuck-detection falls back to spawned_at).
    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert_eq!(a.last_activity_at, None);

    // First tool event sets it.
    store
        .touch_agent_activity("a-3".into(), ts(10))
        .await
        .unwrap();
    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert_eq!(a.last_activity_at, Some(ts(10)));

    // Forward progress advances it.
    store
        .touch_agent_activity("a-3".into(), ts(25))
        .await
        .unwrap();
    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert_eq!(a.last_activity_at, Some(ts(25)));

    // A stale/out-of-order poll must NOT rewind the high-water mark.
    store
        .touch_agent_activity("a-3".into(), ts(15))
        .await
        .unwrap();
    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert_eq!(
        a.last_activity_at,
        Some(ts(25)),
        "monotonic — never rewinds"
    );
}

// ── messages ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn message_insert_list_filter() {
    let (_dir, store) = fresh_store().await;
    let m1 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "hello b".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let _m2 = store
        .insert_message(NewMessage {
            from_agent: "b".into(),
            to_agent: "a".into(),
            kind: "note".into(),
            body: "hi a".into(),
            sent_at: ts(2),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    let to_b = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("b".into()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(to_b.len(), 1);
    assert_eq!(to_b[0].id, m1.id);
    assert_eq!(to_b[0].body, "hello b");
    assert!(to_b[0].delivered_at.is_none());
}

#[tokio::test]
async fn message_search_fts5() {
    let (_dir, store) = fresh_store().await;
    store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "schedule a meeting tomorrow about the planner".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "just a chatty hello, nothing planned".into(),
            sent_at: ts(2),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    let hits = store.search_messages("planner".into()).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("planner"));

    // porter stem: "planning" should fold to the same stem.
    let hits = store.search_messages("planned".into()).await.unwrap();
    assert_eq!(hits.len(), 1);
}

/// Malformed / hostile FTS5 input must never raise a SQLite syntax error (which
/// used to surface as an HTTP 500 leaking the raw SQL message). The query is
/// sanitized into quoted phrase tokens, so these all return Ok — either with
/// the matching rows (token text is still matched literally) or with no rows.
#[tokio::test]
async fn message_search_fts5_malformed_input_never_errors() {
    let (_dir, store) = fresh_store().await;
    store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "schedule a meeting about the planner".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    // Inputs that previously produced FTS5 syntax errors. Each must be Ok(_).
    for q in [
        "\"",            // lone unbalanced quote
        "*",             // bare prefix operator
        "planner*",      // trailing prefix operator
        "col:planner",   // column filter syntax
        "foo AND",       // dangling boolean keyword
        "NEAR(a b)",     // NEAR with no closing context
        "(planner",      // unbalanced paren
        "^planner",      // first-token operator
        "a -b",          // NOT/exclusion operator
        "",              // empty
        "   ",           // whitespace only
        "()-^:*\"",      // nothing but operators
    ] {
        let res = store.search_messages(q.into()).await;
        assert!(res.is_ok(), "query {q:?} should not error, got {res:?}");
    }

    // Token text inside an operator-laden query is still matched literally.
    let hits = store.search_messages("planner*".into()).await.unwrap();
    assert_eq!(hits.len(), 1, "prefix-operator input still finds the row");

    // A query of only special characters yields no rows (empty MATCH avoided).
    let hits = store.search_messages("*".into()).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn mark_delivered_only_undelivered() {
    let (_dir, store) = fresh_store().await;
    let m1 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "one".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let m2 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "two".into(),
            sent_at: ts(2),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    store.mark_delivered(vec![m1.id], ts(10)).await.unwrap();

    let pending = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("b".into()),
            only_undelivered: true,
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, m2.id);
}

#[tokio::test]
async fn mark_read_sets_timestamp_and_returns_ids() {
    let (_dir, store) = fresh_store().await;
    let m1 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "one".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let m2 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "two".into(),
            sent_at: ts(2),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    let marked = store
        .mark_read(vec![m1.id, m2.id], "b".into(), ts(10))
        .await
        .unwrap();
    assert_eq!(marked.len(), 2);
    assert!(marked.contains(&m1.id) && marked.contains(&m2.id));

    let rows = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("b".into()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(rows.iter().all(|r| r.read_at == Some(ts(10))));
    // #20: reading also stamps delivered_at (an MCP read IS delivery), so
    // only_undelivered self-heals and prune_expired can reclaim read mail.
    assert!(rows.iter().all(|r| r.delivered_at == Some(ts(10))));
    let undelivered = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("b".into()),
            only_undelivered: true,
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        undelivered.is_empty(),
        "read messages are now delivered → excluded from only_undelivered"
    );

    // Idempotent: second call returns empty (read_at already set).
    let again = store
        .mark_read(vec![m1.id, m2.id], "b".into(), ts(99))
        .await
        .unwrap();
    assert!(again.is_empty());
}

#[tokio::test]
async fn mark_read_refuses_cross_agent() {
    let (_dir, store) = fresh_store().await;
    let m1 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "for b only".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    // Wrong recipient: c tries to mark b's mail read.
    let marked = store
        .mark_read(vec![m1.id], "c".into(), ts(10))
        .await
        .unwrap();
    assert!(marked.is_empty(), "cross-agent mark must be a no-op");

    let row = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("b".into()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(row.read_at.is_none(), "row stayed unread");
}

/// Regression: a flat `IN (...)` over the whole id list blows SQLite's
/// `SQLITE_MAX_VARIABLE_NUMBER` (999 on the default build) and surfaces as a
/// 500. `mark_read` now chunks at 900 ids/call, so a batch well past one chunk
/// must succeed and mark every row.
#[tokio::test]
async fn mark_read_chunks_large_id_batch() {
    let (_dir, store) = fresh_store().await;
    // > 2 chunks of 900 so the batching loop runs at least three times.
    const N: usize = 2050;
    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        let m = store
            .insert_message(NewMessage {
                from_agent: "a".into(),
                to_agent: "b".into(),
                kind: "note".into(),
                body: format!("m{i}"),
                sent_at: ts(i as i64 + 1),
                in_reply_to: None,
                meta: None,
            })
            .await
            .unwrap();
        ids.push(m.id);
    }

    let marked = store
        .mark_read(ids.clone(), "b".into(), ts(10_000))
        .await
        .expect("large batch must not hit the variable cap");
    assert_eq!(marked.len(), N, "every id marked exactly once");
    let marked_set: std::collections::HashSet<i64> = marked.into_iter().collect();
    assert!(
        ids.iter().all(|id| marked_set.contains(id)),
        "all inserted ids present in the returned set"
    );

    // Idempotent across chunks: a second pass marks nothing.
    let again = store.mark_read(ids, "b".into(), ts(20_000)).await.unwrap();
    assert!(again.is_empty(), "second pass is a no-op");

    let unread = store.count_unread("b".into()).await.unwrap();
    assert_eq!(unread, 0, "no rows left unread after the bulk mark");
}

#[tokio::test]
async fn count_unread_excludes_read() {
    let (_dir, store) = fresh_store().await;
    let m1 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "one".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let _m2 = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "two".into(),
            sent_at: ts(2),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    // Unrelated recipient — must not count.
    let _other = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "c".into(),
            kind: "note".into(),
            body: "for c".into(),
            sent_at: ts(3),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    assert_eq!(store.count_unread("b".into()).await.unwrap(), 2);

    store
        .mark_read(vec![m1.id], "b".into(), ts(10))
        .await
        .unwrap();
    assert_eq!(store.count_unread("b".into()).await.unwrap(), 1);
    assert_eq!(store.count_unread("c".into()).await.unwrap(), 1);
}

#[tokio::test]
async fn consume_wakes_atomically_returns_wake_ids_and_marks_read() {
    // M6f: consume_wakes is the wake_check primary signal. It must
    // (a) only touch kind="wake", (b) only touch unread, (c) only
    // touch this agent, (d) mark read in the same transaction as the
    // SELECT, and (e) be idempotent (second call returns []).
    let (_dir, store) = fresh_store().await;
    let wake1 = store
        .insert_message(NewMessage {
            from_agent: "system".into(),
            to_agent: "critic".into(),
            kind: "wake".into(),
            body: "blackboard `frontend.done` updated".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let _note = store
        .insert_message(NewMessage {
            from_agent: "frontend".into(),
            to_agent: "critic".into(),
            kind: "note".into(),
            body: "fyi".into(),
            sent_at: ts(2),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let wake2 = store
        .insert_message(NewMessage {
            from_agent: "system".into(),
            to_agent: "critic".into(),
            kind: "wake".into(),
            body: "blackboard `backend.done` updated".into(),
            sent_at: ts(3),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    // Different agent — must not be touched.
    let other_wake = store
        .insert_message(NewMessage {
            from_agent: "system".into(),
            to_agent: "test".into(),
            kind: "wake".into(),
            body: "for test".into(),
            sent_at: ts(4),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    let ids = store.consume_wakes("critic".into(), ts(100)).await.unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&wake1.id));
    assert!(ids.contains(&wake2.id));

    // Second call: nothing left to consume.
    let ids_again = store.consume_wakes("critic".into(), ts(101)).await.unwrap();
    assert!(ids_again.is_empty(), "consume_wakes must be idempotent");

    // Other agent's wake is untouched.
    let other_ids = store.consume_wakes("test".into(), ts(102)).await.unwrap();
    assert_eq!(other_ids, vec![other_wake.id]);

    // count_unread still sees the note (kind="note") for critic.
    assert_eq!(store.count_unread("critic".into()).await.unwrap(), 1);
}

#[tokio::test]
async fn insert_and_list_round_trip_in_reply_to() {
    let (_dir, store) = fresh_store().await;
    let parent = store
        .insert_message(NewMessage {
            from_agent: "a".into(),
            to_agent: "b".into(),
            kind: "note".into(),
            body: "first ping".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let reply = store
        .insert_message(NewMessage {
            from_agent: "b".into(),
            to_agent: "a".into(),
            kind: "note".into(),
            body: "pong".into(),
            sent_at: ts(2),
            in_reply_to: Some(parent.id),
            meta: None,
        })
        .await
        .unwrap();
    assert_eq!(reply.in_reply_to, Some(parent.id));

    let listed = store
        .list_messages(ListMessagesOpts {
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    let reply_row = listed.iter().find(|r| r.id == reply.id).unwrap();
    assert_eq!(reply_row.in_reply_to, Some(parent.id));
    let parent_row = listed.iter().find(|r| r.id == parent.id).unwrap();
    assert!(parent_row.in_reply_to.is_none());
}

// ── blackboard ───────────────────────────────────────────────────────────

/// Regression guard for the put-then-delete tombstone bug: `blackboard_paths_present`
/// must report a key as ABSENT once its latest op is a `delete` — otherwise the
/// only production caller (`list_agents` handoff detection) keeps a deleted
/// handoff key "present", freezing a DAG node green and suppressing
/// `handoff_missing`. Verified live (delete left the path's write row in the
/// append-only ledger, so a bare `SELECT DISTINCT path` still matched).
#[tokio::test]
async fn paths_present_excludes_deleted_tombstones() {
    let (_dir, store) = fresh_store().await;
    let sig = "ws/main/reviewer.done";

    // 1) write → present
    store.insert_blackboard_op(bb(sig, "PASS", ts(1))).await.unwrap();
    let present = store
        .blackboard_paths_present(vec![sig.into()])
        .await
        .unwrap();
    assert!(present.contains(sig), "written key must be present");

    // 2) delete → ABSENT (the bug: it used to stay present)
    store
        .record_blackboard_delete(Some("u".into()), sig.into(), ts(2))
        .await
        .unwrap();
    let present = store
        .blackboard_paths_present(vec![sig.into()])
        .await
        .unwrap();
    assert!(
        !present.contains(sig),
        "deleted key must be absent (latest op is a delete tombstone)"
    );

    // 3) re-write after delete → present again (delete isn't permanent)
    store.insert_blackboard_op(bb(sig, "PASS again", ts(3))).await.unwrap();
    let present = store
        .blackboard_paths_present(vec![sig.into()])
        .await
        .unwrap();
    assert!(
        present.contains(sig),
        "re-written key must be present again after a prior delete"
    );

    // 4) a never-written key is absent; a multi-key probe returns only live ones
    let present = store
        .blackboard_paths_present(vec![sig.into(), "ws/main/never.done".into()])
        .await
        .unwrap();
    assert!(present.contains(sig));
    assert!(!present.contains("ws/main/never.done"));
}

/// fusion isolation guard: `list_blackboard_ops_scoped(Some(prefix))` must
/// return ONLY keys at `<prefix>` or under `<prefix>/…`, never a sibling
/// direction's keys. This is the single mechanism that lets a fusion
/// competition hide each contestant's blackboard from the others while a
/// collaborative direction (whose workers share one prefix) stays mutually
/// visible. `None` keeps the historical global listing. Mirrors the live
/// spike that confirmed the un-scoped global GET leaks across directions.
#[tokio::test]
async fn list_blackboard_ops_scoped_isolates_by_prefix() {
    let (_dir, store) = fresh_store().await;
    // Two contestant directions under one workspace, plus an unrelated ws.
    store.insert_blackboard_op(bb("ws1/alpha/secret.md", "A", ts(1))).await.unwrap();
    store.insert_blackboard_op(bb("ws1/alpha/sub/deep.md", "A2", ts(2))).await.unwrap();
    store.insert_blackboard_op(bb("ws1/beta/secret.md", "B", ts(3))).await.unwrap();
    store.insert_blackboard_op(bb("ws2/main/other.md", "C", ts(4))).await.unwrap();

    // Scoped to alpha: sees alpha's own key + nested key, NOT beta or ws2.
    let alpha = store
        .list_blackboard_ops_scoped(Some("ws1/alpha".into()))
        .await
        .unwrap();
    let paths: Vec<&str> = alpha.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"ws1/alpha/secret.md"), "own key visible");
    assert!(paths.contains(&"ws1/alpha/sub/deep.md"), "nested own key visible");
    assert!(!paths.contains(&"ws1/beta/secret.md"), "sibling direction key LEAKED");
    assert!(!paths.contains(&"ws2/main/other.md"), "other workspace key LEAKED");
    assert_eq!(alpha.len(), 2, "scoped list must be exactly alpha's two keys");

    // Scoped to beta: only beta's single key.
    let beta = store
        .list_blackboard_ops_scoped(Some("ws1/beta".into()))
        .await
        .unwrap();
    assert_eq!(beta.len(), 1);
    assert_eq!(beta[0].path, "ws1/beta/secret.md");

    // None = global: every key across both workspaces (historical behaviour).
    let all = store.list_blackboard_ops_scoped(None).await.unwrap();
    assert_eq!(all.len(), 4, "global list returns all four keys");

    // A prefix must not match a longer sibling by string-prefix alone:
    // `ws1/alph` must NOT pick up `ws1/alpha/...` (GLOB boundary is `/`).
    let near = store
        .list_blackboard_ops_scoped(Some("ws1/alph".into()))
        .await
        .unwrap();
    assert!(near.is_empty(), "partial-segment prefix must not match alpha");
}

/// fusion batch CRUD: create binds N contestant directions, list returns it,
/// set_fusion_judge attaches the judge + flips to 'judging', set_fusion_status
/// advances to 'done'. Confirms migration 0026 applied and the JSON id array
/// round-trips.
#[tokio::test]
async fn fusion_batch_crud_roundtrip() {
    let (_dir, store) = fresh_store().await;
    let ws = store
        .create_workspace(
            NewWorkspace {
                name: "fusion-ws".into(),
                cwd: "/tmp/fws".into(),
                accent: None,
            },
            ts(1),
        )
        .await
        .unwrap();

    let batch = store
        .create_fusion_batch(
            NewFusionBatch {
                workspace_id: ws.id.clone(),
                slug: "login-api".into(),
                need: "implement JWT login".into(),
                contestant_thread_ids: vec!["t-alpha".into(), "t-beta".into(), "t-gamma".into()],
                check_cmd: None,
            },
            ts(1),
        )
        .await
        .unwrap();
    assert_eq!(batch.status, "running");
    assert_eq!(batch.contestant_thread_ids.len(), 3);
    assert!(batch.judge_thread_id.is_none());

    // list returns the alive batch with its contestants intact.
    let listed = store.list_fusion_batches(ws.id.clone()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].contestant_thread_ids, batch.contestant_thread_ids);
    assert_eq!(listed[0].need, "implement JWT login");

    // Enter judge stage: the caller atomically claims running→judging (CAS),
    // then set_fusion_judge records the judge thread id (no longer flips status).
    assert_eq!(
        store
            .transition_fusion_status(batch.id.clone(), "running".into(), "judging".into())
            .await
            .unwrap(),
        1
    );
    store
        .set_fusion_judge(batch.id.clone(), "t-judge".into())
        .await
        .unwrap();
    let listed = store.list_fusion_batches(ws.id.clone()).await.unwrap();
    assert_eq!(listed[0].judge_thread_id.as_deref(), Some("t-judge"));
    assert_eq!(listed[0].status, "judging");

    // advance to done.
    store
        .set_fusion_status(batch.id.clone(), "done".into())
        .await
        .unwrap();
    let listed = store.list_fusion_batches(ws.id.clone()).await.unwrap();
    assert_eq!(listed[0].status, "done");
}

#[tokio::test]
async fn blackboard_insert_and_history() {
    let (_dir, store) = fresh_store().await;
    store
        .insert_blackboard_op(NewBlackboardOp {
            agent_id: Some("a".into()),
            op: "write".into(),
            path: "tasks.md".into(),
            content: "- [ ] first\n".into(),
            sha256: "abc".into(),
            at: ts(1),
        })
        .await
        .unwrap();
    store
        .insert_blackboard_op(NewBlackboardOp {
            agent_id: Some("a".into()),
            op: "write".into(),
            path: "tasks.md".into(),
            content: "- [x] first\n- [ ] second\n".into(),
            sha256: "def".into(),
            at: ts(2),
        })
        .await
        .unwrap();
    // Different file.
    store
        .insert_blackboard_op(NewBlackboardOp {
            agent_id: None,
            op: "external".into(),
            path: "notes/scratch.md".into(),
            content: "scratch".into(),
            sha256: "ghi".into(),
            at: ts(3),
        })
        .await
        .unwrap();

    let history = store
        .list_blackboard_ops(Some("tasks.md".into()))
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    // ORDER BY id DESC — newest first.
    assert_eq!(history[0].sha256, "def");

    let latest = store.list_blackboard_ops(None).await.unwrap();
    assert_eq!(latest.len(), 2, "one row per distinct path");
    let paths: Vec<&str> = latest.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"tasks.md"));
    assert!(paths.contains(&"notes/scratch.md"));
}

// ── recordings ───────────────────────────────────────────────────────────

#[tokio::test]
async fn recording_start_then_finalize() {
    let (_dir, store) = fresh_store().await;
    store
        .record_recording_start(NewRecording {
            id: "rec-1".into(),
            agent_id: "a-1".into(),
            path: "/tmp/rec-1.cast".into(),
            started_at: ts(0),
            cols: 120,
            rows: 32,
        })
        .await
        .unwrap();

    let r = store.get_recording("rec-1".into()).await.unwrap().unwrap();
    assert_eq!(r.agent_id, "a-1");
    assert_eq!(r.cols, 120);
    assert!(r.finalized_at.is_none());
    assert!(r.duration_ms.is_none());

    store
        .record_recording_finalize("rec-1".into(), ts(100), 100, 17)
        .await
        .unwrap();
    let r = store.get_recording("rec-1".into()).await.unwrap().unwrap();
    assert_eq!(r.finalized_at, Some(ts(100)));
    assert_eq!(r.duration_ms, Some(100));
    assert_eq!(r.last_seq, Some(17));

    // Idempotent: second finalize is a no-op (first non-NULL wins).
    store
        .record_recording_finalize("rec-1".into(), ts(999), 999, 999)
        .await
        .unwrap();
    let r = store.get_recording("rec-1".into()).await.unwrap().unwrap();
    assert_eq!(r.finalized_at, Some(ts(100)));
}

#[tokio::test]
async fn recordings_listed_by_agent() {
    let (_dir, store) = fresh_store().await;
    for (i, agent) in [("rec-1", "a-1"), ("rec-2", "a-1"), ("rec-3", "a-2")] {
        store
            .record_recording_start(NewRecording {
                id: i.into(),
                agent_id: agent.into(),
                path: format!("/tmp/{}.cast", i),
                started_at: ts(0),
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
    }
    let for_a1 = store.list_recordings(Some("a-1".into())).await.unwrap();
    assert_eq!(for_a1.len(), 2);
    assert!(for_a1.iter().all(|r| r.agent_id == "a-1"));

    let all = store.list_recordings(None).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn store_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("swarmx.db");
    {
        let store = Store::open(&db_path).await.unwrap();
        store
            .record_agent_spawn(NewAgent {
                id: "persist-1".into(),
                cli: "claude".into(),
                role: "x".into(),
                workspace: "/tmp/x".into(),
                spawned_at: ts(0),
                workspace_id: None,
                spell_run_id: None,
                thread_id: None,
            })
            .await
            .unwrap();
    }
    let store = Store::open(&db_path).await.unwrap();
    let agents = store.list_agents().await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].id, "persist-1");
}

// ── orphan settlement on server restart ──────────────────────────────────

#[tokio::test]
async fn mark_orphan_agents_killed_only_alive_rows() {
    let (_dir, store) = fresh_store().await;
    store
        .record_agent_spawn(NewAgent {
            id: "alive-1".into(),
            cli: "claude".into(),
            role: "claude".into(),
            workspace: "/tmp/x".into(),
            spawned_at: ts(0),
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();
    store
        .record_agent_spawn(NewAgent {
            id: "killed-1".into(),
            cli: "claude".into(),
            role: "claude".into(),
            workspace: "/tmp/y".into(),
            spawned_at: ts(0),
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();
    store
        .record_agent_kill("killed-1".into(), ts(50))
        .await
        .unwrap();

    let n = store.mark_orphan_agents_killed(ts(100)).await.unwrap();
    assert_eq!(n, 1, "only the alive-1 row should be updated");

    let agents = store.list_agents().await.unwrap();
    let alive = agents.iter().find(|a| a.id == "alive-1").unwrap();
    let killed = agents.iter().find(|a| a.id == "killed-1").unwrap();
    assert_eq!(alive.killed_at, Some(ts(100)));
    assert_eq!(killed.killed_at, Some(ts(50)), "prior kill timestamp wins");
}

#[tokio::test]
async fn mark_orphan_recordings_finalized_only_live_rows() {
    let (_dir, store) = fresh_store().await;
    store
        .record_recording_start(NewRecording {
            id: "live".into(),
            agent_id: "a".into(),
            path: "/tmp/live.cast".into(),
            started_at: ts(0),
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    store
        .record_recording_start(NewRecording {
            id: "done".into(),
            agent_id: "a".into(),
            path: "/tmp/done.cast".into(),
            started_at: ts(0),
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    store
        .record_recording_finalize("done".into(), ts(50), 50, 7)
        .await
        .unwrap();

    let n = store
        .mark_orphan_recordings_finalized(ts(100))
        .await
        .unwrap();
    assert_eq!(n, 1, "only the live row should be settled");

    let live = store.get_recording("live".into()).await.unwrap().unwrap();
    assert_eq!(live.finalized_at, Some(ts(100)));
    // duration_ms is backfilled from the wall-clock span (finalized_at -
    // started_at = 100) so restart-orphan recordings aren't shown with no
    // duration in the Replays list; last_seq stays NULL (reattach treats
    // NULL as "replay from head"). See mark_orphan_recordings_finalized.
    assert_eq!(live.duration_ms, Some(100));
    assert!(live.last_seq.is_none());

    let done = store.get_recording("done".into()).await.unwrap().unwrap();
    assert_eq!(done.finalized_at, Some(ts(50)));
    assert_eq!(done.duration_ms, Some(50));
    assert_eq!(done.last_seq, Some(7));
}

// ── shim pid/pgid ledger + orphan process candidates (migration 0029) ───────

#[tokio::test]
async fn record_agent_shim_pid_first_write_wins() {
    let (_dir, store) = fresh_store().await;
    store.record_agent_spawn(spawn_agent_row("p-1")).await.unwrap();

    // First record lands; a second one must not clobber it.
    assert!(store.record_agent_shim_pid("p-1".into(), 4242, Some(4242)).await.unwrap());
    assert!(!store.record_agent_shim_pid("p-1".into(), 9999, Some(9999)).await.unwrap());
    // Unknown agent id → nothing written (the reaper retries next sweep).
    assert!(!store.record_agent_shim_pid("nope".into(), 1, None).await.unwrap());

    // Read back via the orphan-candidate query: mark killed inside the window
    // and confirm the ORIGINAL pid survived.
    store.record_agent_kill("p-1".into(), ts(100)).await.unwrap();
    let cands = store.orphaned_agent_processes(ts(50)).await.unwrap();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "p-1");
    assert_eq!(cands[0].shim_pid, 4242);
    assert_eq!(cands[0].shim_pgid, Some(4242));
}

#[tokio::test]
async fn orphaned_agent_processes_selects_only_boot_settled_rows_with_pid() {
    let (_dir, store) = fresh_store().await;
    for id in ["no-pid", "in-window", "old-kill", "shim-exited", "still-alive"] {
        store.record_agent_spawn(spawn_agent_row(id)).await.unwrap();
    }
    // Every row except "no-pid" gets a process identity.
    for id in ["in-window", "old-kill", "shim-exited", "still-alive"] {
        assert!(store.record_agent_shim_pid(id.into(), 7777, None).await.unwrap());
    }
    // "shim-exited": the pump recorded the shim's death before the kill.
    store.record_shim_exit("shim-exited".into(), 1, ts(40)).await.unwrap();
    // Settle the kills: in-window vs long-past, plus the never-killed row.
    store.record_agent_kill("no-pid".into(), ts(100)).await.unwrap();
    store.record_agent_kill("in-window".into(), ts(100)).await.unwrap();
    store.record_agent_kill("shim-exited".into(), ts(100)).await.unwrap();
    store.record_agent_kill("old-kill".into(), ts(10)).await.unwrap();

    let cands = store.orphaned_agent_processes(ts(50)).await.unwrap();
    let ids: Vec<&str> = cands.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        ["in-window"],
        "only a pid-carrying, no-shim-exit row killed inside the window qualifies"
    );

    // A wider window picks up the old kill too — the caller's window is the
    // only thing standing between us and a recycled pid from an old session.
    // (All rows share spawned_at = ts(0), so compare as a set, not a sequence.)
    let cands = store.orphaned_agent_processes(ts(0)).await.unwrap();
    let mut ids: Vec<&str> = cands.iter().map(|c| c.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["in-window", "old-kill"]);
}

// ── retention / prune (F5) ──────────────────────────────────────────────────

fn bb(path: &str, content: &str, at: i64) -> NewBlackboardOp {
    NewBlackboardOp {
        agent_id: Some("a".into()),
        op: "write".into(),
        path: path.into(),
        content: content.into(),
        sha256: format!("sha-{content}"),
        at,
    }
}

#[tokio::test]
async fn prune_keeps_every_load_bearing_row() {
    let (dir, store) = fresh_store().await;
    let cutoff = ts(1000); // rows with timestamp < cutoff are "old"

    // ── blackboard ──────────────────────────────────────────────────────
    // path "p": two old superseded rows + one recent latest → 2 deletable.
    store
        .insert_blackboard_op(bb("p", "p-v0", ts(0)))
        .await
        .unwrap();
    store
        .insert_blackboard_op(bb("p", "p-v1", ts(100)))
        .await
        .unwrap();
    store
        .insert_blackboard_op(bb("p", "p-v2", ts(2000)))
        .await
        .unwrap();
    // path "q": single OLD row — it's the latest for q, must be KEPT despite age.
    store
        .insert_blackboard_op(bb("q", "q-v0", ts(0)))
        .await
        .unwrap();
    // path "r": old superseded + old latest → only the superseded one deletable.
    store
        .insert_blackboard_op(bb("r", "r-v0", ts(0)))
        .await
        .unwrap();
    store
        .insert_blackboard_op(bb("r", "r-v1", ts(50)))
        .await
        .unwrap();

    // ── messages ────────────────────────────────────────────────────────
    let id = |m: MessageRecord| m.id;
    // m1: old normal note, delivered+read → deletable.
    let m1 = id(store
        .insert_message(NewMessage {
            from_agent: "x".into(),
            to_agent: "a".into(),
            kind: "note".into(),
            body: "old read".into(),
            sent_at: ts(0),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap());
    store.mark_delivered(vec![m1], ts(1)).await.unwrap();
    store.mark_read(vec![m1], "a".into(), ts(2)).await.unwrap();
    // m2: old normal note, NOT delivered/read → KEPT (conservative).
    store
        .insert_message(NewMessage {
            from_agent: "x".into(),
            to_agent: "a".into(),
            kind: "note".into(),
            body: "old unread".into(),
            sent_at: ts(0),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    // m3: old wake, UN-consumed (different agent so consume below misses it) → KEPT.
    store
        .insert_message(NewMessage {
            from_agent: "x".into(),
            to_agent: "keep".into(),
            kind: "wake".into(),
            body: "pending wake".into(),
            sent_at: ts(0),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    // m4: old wake, consumed → deletable.
    store
        .insert_message(NewMessage {
            from_agent: "x".into(),
            to_agent: "cons".into(),
            kind: "wake".into(),
            body: "spent wake".into(),
            sent_at: ts(0),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    let consumed = store.consume_wakes("cons".into(), ts(3)).await.unwrap();
    assert_eq!(consumed.len(), 1, "only the 'cons' wake is consumed");
    // m5: recent normal note, delivered+read → KEPT (newer than cutoff).
    let m5 = id(store
        .insert_message(NewMessage {
            from_agent: "x".into(),
            to_agent: "a".into(),
            kind: "note".into(),
            body: "recent".into(),
            sent_at: ts(2000),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap());
    store.mark_delivered(vec![m5], ts(2001)).await.unwrap();
    store
        .mark_read(vec![m5], "a".into(), ts(2002))
        .await
        .unwrap();
    // m6 parent + m7 child: both old, delivered+read. m6 is referenced by m7
    // → m6 KEPT this pass (FK-safe), m7 deletable.
    let m6 = id(store
        .insert_message(NewMessage {
            from_agent: "x".into(),
            to_agent: "a".into(),
            kind: "note".into(),
            body: "parent".into(),
            sent_at: ts(0),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap());
    store.mark_delivered(vec![m6], ts(1)).await.unwrap();
    store.mark_read(vec![m6], "a".into(), ts(2)).await.unwrap();
    let m7 = id(store
        .insert_message(NewMessage {
            from_agent: "x".into(),
            to_agent: "a".into(),
            kind: "note".into(),
            body: "child".into(),
            sent_at: ts(0),
            in_reply_to: Some(m6),
            meta: None,
        })
        .await
        .unwrap());
    store.mark_delivered(vec![m7], ts(1)).await.unwrap();
    store.mark_read(vec![m7], "a".into(), ts(2)).await.unwrap();

    // ── recordings ──────────────────────────────────────────────────────
    let cast = dir.path().join("rec1.cast");
    std::fs::write(&cast, b"cast bytes").unwrap();
    store
        .record_recording_start(NewRecording {
            id: "rec1".into(),
            agent_id: "a".into(),
            path: cast.to_string_lossy().into(),
            started_at: ts(0),
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    store
        .record_recording_finalize("rec1".into(), ts(10), 10, 1)
        .await
        .unwrap();
    // rec2: old but LIVE (not finalized) → KEPT.
    store
        .record_recording_start(NewRecording {
            id: "rec2".into(),
            agent_id: "a".into(),
            path: "/tmp/rec2.cast".into(),
            started_at: ts(0),
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    // rec3: recent + finalized → KEPT.
    store
        .record_recording_start(NewRecording {
            id: "rec3".into(),
            agent_id: "a".into(),
            path: "/tmp/rec3.cast".into(),
            started_at: ts(2000),
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    store
        .record_recording_finalize("rec3".into(), ts(2010), 10, 1)
        .await
        .unwrap();

    // ── prune ───────────────────────────────────────────────────────────
    let stats = store.prune_expired(cutoff).await.unwrap();
    assert_eq!(stats.blackboard_ops, 3, "p:2 + r:1 superseded-old rows");
    assert_eq!(
        stats.messages, 3,
        "m1 (read) + m4 (consumed wake) + m7 (child)"
    );
    assert_eq!(stats.recordings, 1, "rec1 old+finalized");
    assert_eq!(stats.recording_files_removed, 1, "rec1.cast unlinked");

    // blackboard: all three paths still discoverable, latest content intact.
    let latest = store.list_blackboard_ops(None).await.unwrap();
    let mut paths: Vec<&str> = latest.iter().map(|r| r.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["p", "q", "r"], "no path lost discovery");
    let p_rows = store.list_blackboard_ops(Some("p".into())).await.unwrap();
    assert_eq!(p_rows.len(), 1, "p keeps only its latest row");
    assert_eq!(p_rows[0].content, "p-v2");

    // recordings: rec1 row + file gone; rec2 (live) + rec3 (recent) kept.
    assert!(!cast.exists(), "pruned .cast file removed from disk");
    assert!(store.get_recording("rec1".into()).await.unwrap().is_none());
    assert!(store.get_recording("rec2".into()).await.unwrap().is_some());
    assert!(store.get_recording("rec3".into()).await.unwrap().is_some());

    // messages: kept = m2 (unread), m3 (pending wake), m5 (recent), m6 (parent).
    let kept_a = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("a".into()),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    let bodies: Vec<&str> = kept_a.iter().map(|m| m.body.as_str()).collect();
    assert!(bodies.contains(&"old unread"));
    assert!(bodies.contains(&"recent"));
    assert!(bodies.contains(&"parent"));
    assert!(
        !bodies.contains(&"old read"),
        "delivered+read old note pruned"
    );
    assert!(!bodies.contains(&"child"), "child pruned");
    let pending = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("keep".into()),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "un-consumed wake must survive");

    // Second pass: m6 is now unreferenced (m7 gone) → ages out. Demonstrates
    // the FK-safe staged deletion documented on prune_expired.
    let stats2 = store.prune_expired(cutoff).await.unwrap();
    assert_eq!(stats2.messages, 1, "orphaned parent ages out next pass");
    assert_eq!(stats2.blackboard_ops, 0);
    assert_eq!(stats2.recordings, 0);
}

// ── threads (per-workspace directions) ───────────────────────────────────

#[tokio::test]
async fn threads_crud_roundtrip() {
    let (_dir, store) = fresh_store().await;
    let ws = store
        .create_workspace(
            NewWorkspace {
                name: "proj".into(),
                cwd: "/tmp/proj".into(),
                accent: None,
            },
            ts(0),
        )
        .await
        .unwrap();

    // create the main thread (shared) + a second direction.
    let main = store
        .create_thread(
            NewThread {
                workspace_id: ws.id.clone(),
                slug: "main".into(),
                name: Some("主线".into()),
                isolation: "shared".into(),
                branch: None,
                cwd: ws.cwd.clone(),
                state: "ready".into(),
            },
            ts(1),
        )
        .await
        .unwrap();
    let dark = store
        .create_thread(
            NewThread {
                workspace_id: ws.id.clone(),
                slug: "t-abc123".into(),
                name: None,
                isolation: "shared".into(),
                branch: None,
                cwd: ws.cwd.clone(),
                state: "ready".into(),
            },
            ts(2),
        )
        .await
        .unwrap();

    // list: oldest first (main before dark).
    let listed = store.list_threads(ws.id.clone()).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].slug, "main");
    assert_eq!(listed[1].slug, "t-abc123");
    assert_eq!(listed[1].name, None);

    // update: AI names the direction + upgrades it to a worktree.
    store
        .update_thread(
            dark.id.clone(),
            Some("深色模式".into()),
            Some("dark-mode".into()),
            Some("worktree".into()),
            Some("dark-mode".into()),
            Some("/tmp/proj-dark-mode".into()),
            Some("ready".into()),
        )
        .await
        .unwrap();
    let got = store.get_thread(dark.id.clone()).await.unwrap().unwrap();
    assert_eq!(got.name.as_deref(), Some("深色模式"));
    assert_eq!(got.slug, "dark-mode");
    assert_eq!(got.isolation, "worktree");
    assert_eq!(got.branch.as_deref(), Some("dark-mode"));
    assert_eq!(got.cwd, "/tmp/proj-dark-mode");
    // partial update leaves other columns untouched.
    assert_eq!(got.workspace_id, ws.id);

    // agent → thread reverse lookup.
    store
        .record_agent_spawn(NewAgent {
            id: "ag-1".into(),
            cli: "claude".into(),
            role: "orchestrator".into(),
            workspace: ws.cwd.clone(),
            spawned_at: ts(3),
            workspace_id: Some(ws.id.clone()),
            spell_run_id: None,
            thread_id: Some(dark.id.clone()),
        })
        .await
        .unwrap();
    assert_eq!(
        store.get_thread_id_for_agent("ag-1".into()).await.unwrap(),
        Some(dark.id.clone())
    );
    // an agent with no thread → None (= main).
    store
        .record_agent_spawn(NewAgent {
            id: "ag-2".into(),
            cli: "claude".into(),
            role: "x".into(),
            workspace: ws.cwd.clone(),
            spawned_at: ts(4),
            workspace_id: Some(ws.id.clone()),
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        store.get_thread_id_for_agent("ag-2".into()).await.unwrap(),
        None
    );

    // soft-delete frees the slug + drops it from the alive list.
    store
        .soft_delete_thread(main.id.clone(), ts(5))
        .await
        .unwrap();
    let after = store.list_threads(ws.id.clone()).await.unwrap();
    assert_eq!(after.len(), 1, "main soft-deleted; only dark remains");
    assert_eq!(after[0].id, dark.id);
    // slug 'main' is now reusable (alive-only UNIQUE index).
    let main2 = store
        .create_thread(
            NewThread {
                workspace_id: ws.id.clone(),
                slug: "main".into(),
                name: Some("主线2".into()),
                isolation: "shared".into(),
                branch: None,
                cwd: ws.cwd.clone(),
                state: "ready".into(),
            },
            ts(6),
        )
        .await
        .unwrap();
    assert_eq!(main2.slug, "main");
}

// ── re-root: the user's request survives an orchestrator handoff ─────────────
// Regression for fd45c14. Isolating a direction into a worktree kills the
// orchestrator that named it and respawns it under a NEW id. Two store
// guarantees keep the user's opening request from being dropped across that
// swap, so the replacement never re-greets "想干啥?" over a real ask:
//   1. reassign_unread_user_messages — an as-yet-UNREAD `user → old` message is
//      re-addressed to the new orchestrator, and nothing else is disturbed.
//   2. latest_user_message_for_agents — if the old orchestrator already READ the
//      request (it had to, to name the direction), #1 finds nothing to move, so
//      the request body is recovered from here instead.

#[tokio::test]
async fn reassign_unread_moves_only_the_user_request() {
    let (_dir, store) = fresh_store().await;

    // The unanswered request → must move to the new orchestrator.
    let req = store
        .insert_message(NewMessage {
            from_agent: "user".into(),
            to_agent: "orch-old".into(),
            kind: "note".into(),
            body: "build me a login page".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    // An already-READ user message → stays put (it's been answered).
    let answered = store
        .insert_message(NewMessage {
            from_agent: "user".into(),
            to_agent: "orch-old".into(),
            kind: "note".into(),
            body: "earlier, already handled".into(),
            sent_at: ts(2),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    store
        .mark_read(vec![answered.id], "orch-old".into(), ts(3))
        .await
        .unwrap();
    // Agent→orchestrator traffic (not from the user) → stays put.
    store
        .insert_message(NewMessage {
            from_agent: "worker-7".into(),
            to_agent: "orch-old".into(),
            kind: "note".into(),
            body: "backend.done".into(),
            sent_at: ts(4),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    // A different direction's orchestrator (not in the killed set) → stays put.
    store
        .insert_message(NewMessage {
            from_agent: "user".into(),
            to_agent: "orch-other".into(),
            kind: "note".into(),
            body: "unrelated direction".into(),
            sent_at: ts(5),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();

    let moved = store
        .reassign_unread_user_messages(vec!["orch-old".into()], "orch-new".into())
        .await
        .unwrap();
    assert_eq!(moved, 1, "exactly the one unread user request moves");

    // The request is now addressed to the fresh orchestrator…
    let to_new = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("orch-new".into()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(to_new.len(), 1);
    assert_eq!(to_new[0].id, req.id);
    assert_eq!(to_new[0].body, "build me a login page");

    // …and the old orchestrator keeps exactly the read + agent rows, never the
    // request.
    let to_old = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("orch-old".into()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(to_old.len(), 2, "read user msg + agent msg stay put");
    assert!(to_old.iter().all(|m| m.id != req.id));

    // The unrelated direction is untouched.
    let to_other = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("orch-other".into()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(to_other.len(), 1);
}

#[tokio::test]
async fn already_read_request_is_recovered_by_latest_user_message() {
    // The exact fd45c14 orphan: the old orchestrator READ the user's request to
    // name the direction, so by re-root time there is nothing UNREAD left to
    // reassign — a zero move is the bug condition, not a failure.
    let (_dir, store) = fresh_store().await;
    let req = store
        .insert_message(NewMessage {
            from_agent: "user".into(),
            to_agent: "orch-old".into(),
            kind: "note".into(),
            body: "add a dark mode toggle".into(),
            sent_at: ts(1),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    store
        .mark_read(vec![req.id], "orch-old".into(), ts(2))
        .await
        .unwrap();

    let moved = store
        .reassign_unread_user_messages(vec!["orch-old".into()], "orch-new".into())
        .await
        .unwrap();
    assert_eq!(moved, 0, "a read message has nothing left to reassign");

    // …but the request body is still recoverable, so the respawned orchestrator
    // addresses the real ask instead of greeting from scratch.
    let recovered = store
        .latest_user_message_for_agents(vec!["orch-old".into()])
        .await
        .unwrap();
    assert_eq!(recovered.as_deref(), Some("add a dark mode toggle"));
}

// ── delete direction → its blackboard ledgers go too, nothing else ──────────
// Regression for the "blackboard leak on direction delete" bug. Deleting a
// direction must drop `<ws>/<slug>` and everything under `<ws>/<slug>/…`, but
// must NOT touch sibling directions, other workspaces, or keys that merely
// share a string prefix. The store uses GLOB (not LIKE) precisely so a slug
// containing `_` can't act as a single-char wildcard — this pins that choice:
// the `ws/darkXmode/leak.md` decoy would be wrongly deleted by a LIKE impl.

#[tokio::test]
async fn delete_blackboard_prefix_drops_only_the_direction_subtree() {
    let (_dir, store) = fresh_store().await;
    for p in [
        "ws/dark_mode",              // the bare dir key      → deleted
        "ws/dark_mode/ledger.md",    // under it              → deleted
        "ws/dark_mode/sub/notes.md", // deeper under it       → deleted
        "ws/dark_mode-old/x.md",     // shares prefix, not under → survives
        "ws/darkXmode/leak.md",      // `_`-as-LIKE-wildcard footgun → survives
        "ws/light/ledger.md",        // sibling direction     → survives
        "ws2/dark_mode/ledger.md",   // different workspace    → survives
    ] {
        store
            .insert_blackboard_op(NewBlackboardOp {
                agent_id: Some("orch".into()),
                op: "write".into(),
                path: p.into(),
                content: "x".into(),
                sha256: "h".into(),
                at: ts(1),
            })
            .await
            .unwrap();
    }

    let removed = store
        .delete_blackboard_prefix("ws/dark_mode".into())
        .await
        .unwrap();
    assert_eq!(removed, 3, "only the bare key + the two keys under it");

    let mut left: Vec<String> = store
        .list_blackboard_ops(None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.path)
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec![
            // sorted ASCII: 'X' (0x58) precedes '_' (0x5F)
            "ws/darkXmode/leak.md".to_string(),
            "ws/dark_mode-old/x.md".to_string(),
            "ws/light/ledger.md".to_string(),
            "ws2/dark_mode/ledger.md".to_string(),
        ],
        "siblings, other workspaces, and prefix-sharing keys all survive",
    );
}

// ── workers: P0 (F1) typed-handoff columns round-trip (migration 0011) ──────

#[tokio::test]
async fn worker_roundtrip_persists_p0_typed_fields() {
    let (_dir, store) = fresh_store().await;
    // workers.agent_id REFERENCES agents(id) — the agent row must exist first.
    store
        .record_agent_spawn(NewAgent {
            id: "w-1".into(),
            cli: "claude".into(),
            role: "frontend".into(),
            workspace: "/tmp/w".into(),
            spawned_at: ts(0),
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();

    store
        .record_worker(NewWorker {
            agent_id: "w-1".into(),
            parent_agent_id: "orch-1".into(),
            role_label: "Frontend Engineer".into(),
            system_prompt: "build the UI".into(),
            handoff_signal: "ws1/main/frontend.done".into(),
            depends_on_json: "[\"ws1/main/designer.spec\"]".into(),
            spawned_at: ts(1),
            role_slug: "frontend".into(),
            produces_json: "[\"done\"]".into(),
            consumes_json: "[{\"from_role\":\"designer\",\"kind\":\"spec\"}]".into(),
        })
        .await
        .unwrap();

    let map = store.list_workers_by_ids(vec!["w-1".into()]).await.unwrap();
    let w = map.get("w-1").expect("worker row present");
    assert_eq!(w.role_slug, "frontend");
    assert_eq!(w.handoff_signal, "ws1/main/frontend.done");
    assert_eq!(w.depends_on_json, "[\"ws1/main/designer.spec\"]");
    assert_eq!(w.produces_json, "[\"done\"]");
    assert_eq!(
        w.consumes_json,
        "[{\"from_role\":\"designer\",\"kind\":\"spec\"}]"
    );
}

#[tokio::test]
async fn worker_roundtrip_defaults_empty_typed_fields() {
    // A worker with no typed deps/produces (NULL columns) maps back to the
    // documented defaults, not to a panic — back-compat with pre-0011 rows.
    let (_dir, store) = fresh_store().await;
    store
        .record_agent_spawn(NewAgent {
            id: "w-2".into(),
            cli: "codex".into(),
            role: "backend".into(),
            workspace: "/tmp/w".into(),
            spawned_at: ts(0),
            workspace_id: None,
            spell_run_id: None,
            thread_id: None,
        })
        .await
        .unwrap();
    store
        .record_worker(NewWorker {
            agent_id: "w-2".into(),
            parent_agent_id: "orch-1".into(),
            role_label: "Backend".into(),
            system_prompt: "x".into(),
            handoff_signal: String::new(),
            depends_on_json: String::new(),
            spawned_at: ts(1),
            role_slug: String::new(),
            produces_json: String::new(),
            consumes_json: String::new(),
        })
        .await
        .unwrap();
    let map = store.list_workers_by_ids(vec!["w-2".into()]).await.unwrap();
    let w = map.get("w-2").expect("worker row present");
    assert_eq!(w.role_slug, "");
    assert_eq!(w.depends_on_json, "[]");
    assert_eq!(w.produces_json, "[]");
    assert_eq!(w.consumes_json, "[]");
}

// ── agent health / error latch (honesty layer) ──────────────────────────────

fn spawn_agent_row(id: &str) -> NewAgent {
    NewAgent {
        id: id.into(),
        cli: "claude".into(),
        role: "orchestrator".into(),
        workspace: "/tmp/h".into(),
        spawned_at: ts(0),
        workspace_id: None,
        spell_run_id: None,
        thread_id: None,
    }
}

#[tokio::test]
async fn agent_error_records_then_clears() {
    let (_dir, store) = fresh_store().await;
    store.record_agent_spawn(spawn_agent_row("h-1")).await.unwrap();

    // Healthy on spawn.
    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert!(a.last_error.is_none());

    // HealthScanner / watchdog records a soft error.
    store
        .record_agent_error("h-1".into(), "Claude Code 未登录".into(), "auth", ts(5))
        .await
        .unwrap();
    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert_eq!(a.last_error.as_deref(), Some("Claude Code 未登录"));
    assert_eq!(a.last_error_kind.as_deref(), Some("auth"));
    assert_eq!(a.last_error_at, Some(ts(5)));

    // Recovery (e.g. /login in the terminal) clears the latch.
    store.clear_agent_error("h-1".into()).await.unwrap();
    let a = store.list_agents().await.unwrap().pop().unwrap();
    assert!(a.last_error.is_none(), "last_error cleared");
    assert!(a.last_error_kind.is_none());
    assert!(a.last_error_at.is_none());
}

#[tokio::test]
async fn agent_process_dead_distinguishes_soft_error_from_termination() {
    let (_dir, store) = fresh_store().await;
    store.record_agent_spawn(spawn_agent_row("h-2")).await.unwrap();
    store.record_shim_ready("h-2".into(), ts(1)).await.unwrap();

    // Alive (a soft error leaves killed_at/shim_exit_at NULL) → not dead, so the
    // tailer must keep tailing to observe recovery.
    store
        .record_agent_error("h-2".into(), "未登录".into(), "auth", ts(2))
        .await
        .unwrap();
    assert!(
        !store.agent_process_dead("h-2".into()).await.unwrap(),
        "soft error must NOT read as process death"
    );

    // A real shim exit (process gone) → dead, tailer should stop.
    store.record_shim_exit("h-2".into(), 1, ts(3)).await.unwrap();
    assert!(store.agent_process_dead("h-2".into()).await.unwrap());

    // A kill is also terminal even without a shim exit row.
    store.record_agent_spawn(spawn_agent_row("h-3")).await.unwrap();
    assert!(!store.agent_process_dead("h-3".into()).await.unwrap());
    store.record_agent_kill("h-3".into(), ts(4)).await.unwrap();
    assert!(store.agent_process_dead("h-3".into()).await.unwrap());

    // Unknown id → treated as gone (nothing to tail).
    assert!(store.agent_process_dead("nope".into()).await.unwrap());
}

#[tokio::test]
async fn agent_silent_since_ready_reflects_signs_of_life() {
    let (_dir, store) = fresh_store().await;
    store.record_agent_spawn(spawn_agent_row("h-4")).await.unwrap();

    // Fresh, no message / activity / usage / error → silent (watchdog fires).
    assert!(store.agent_silent_since_ready("h-4".into()).await.unwrap());

    // A message FROM the agent is a sign of life → not silent.
    store
        .insert_message(NewMessage {
            from_agent: "h-4".into(),
            to_agent: "user".into(),
            kind: "note".into(),
            body: "hi".into(),
            sent_at: ts(10),
            in_reply_to: None,
            meta: None,
        })
        .await
        .unwrap();
    assert!(!store.agent_silent_since_ready("h-4".into()).await.unwrap());

    // An already-errored agent is never re-flagged by the watchdog.
    store.record_agent_spawn(spawn_agent_row("h-5")).await.unwrap();
    store
        .record_agent_error("h-5".into(), "未登录".into(), "auth", ts(11))
        .await
        .unwrap();
    assert!(!store.agent_silent_since_ready("h-5".into()).await.unwrap());

    // Tool activity (no message) also counts as alive.
    store.record_agent_spawn(spawn_agent_row("h-6")).await.unwrap();
    assert!(store.agent_silent_since_ready("h-6".into()).await.unwrap());
    store.touch_agent_activity("h-6".into(), ts(12)).await.unwrap();
    assert!(!store.agent_silent_since_ready("h-6".into()).await.unwrap());

    // Token usage alone — no message, no tool activity — is the DECISIVE
    // liveness signal (see store.rs doc): a slow-first-turn orchestrator that's
    // talking to the model but hasn't spoken yet must NOT be watchdog-killed.
    // Guards the agent_usage NOT EXISTS branch against silent column/join drift.
    store.record_agent_spawn(spawn_agent_row("h-7")).await.unwrap();
    assert!(store.agent_silent_since_ready("h-7".into()).await.unwrap());
    store
        .insert_agent_usage("h-7".into(), Some("claude".into()), 100, 50, 0, 0, ts(13))
        .await
        .unwrap();
    assert!(
        !store.agent_silent_since_ready("h-7".into()).await.unwrap(),
        "token usage alone must read as alive"
    );
}

// ── stalled = alive + old unread mail + no activity (NeedsYou 「该醒没醒」) ──
// Zero-false-positive is the design bar (0.3.0 cried wolf on a working codex):
// idle-but-waiting workers (no unread) and busy workers (fresh activity) must
// NEVER match. Times below: NOW=1_700_001_000_000, SILENCE=600s →
// cutoff=1_700_000_400_000; "old" = 1_700_000_300_000 (older than cutoff),
// "fresh" = 1_700_000_900_000 (newer than cutoff).

#[tokio::test]
async fn stalled_agents_with_unread_requires_old_unread_and_silence() {
    const NOW: i64 = 1_700_001_000_000;
    const SILENCE: i64 = 600_000;
    const OLD: i64 = 1_700_000_300_000; // waited 700s > SILENCE
    const FRESH: i64 = 1_700_000_900_000; // waited 100s < SILENCE
    let (_dir, store) = fresh_store().await;
    let mail = |to: &str, sent_at: i64| NewMessage {
        from_agent: "orchestrator".into(),
        to_agent: to.into(),
        kind: "note".into(),
        body: "ping".into(),
        sent_at,
        in_reply_to: None,
        meta: None,
    };

    // s-1: alive + old unread + no activity ever → STALLED (the target case:
    // something is waiting for it and it has shown no sign of life).
    store.record_agent_spawn(spawn_agent_row("s-1")).await.unwrap();
    store.record_shim_ready("s-1".into(), OLD).await.unwrap();
    store.insert_message(mail("s-1", OLD)).await.unwrap();

    // s-2: alive + old unread BUT recent activity → NOT stalled (it is
    // working; the mail just hasn't been picked up yet this turn).
    store.record_agent_spawn(spawn_agent_row("s-2")).await.unwrap();
    store.record_shim_ready("s-2".into(), OLD).await.unwrap();
    store.insert_message(mail("s-2", OLD)).await.unwrap();
    store.touch_agent_activity("s-2".into(), FRESH).await.unwrap();

    // s-3: alive + silent, but the unread mail is too FRESH → NOT stalled
    // (the wake chain is allowed this much slack before we suspect it).
    store.record_agent_spawn(spawn_agent_row("s-3")).await.unwrap();
    store.record_shim_ready("s-3".into(), OLD).await.unwrap();
    store.insert_message(mail("s-3", FRESH)).await.unwrap();

    // s-4: alive + silent + NO unread at all → NOT stalled (an idle worker
    // waiting for work is the healthy case, not a stuck one).
    store.record_agent_spawn(spawn_agent_row("s-4")).await.unwrap();
    store.record_shim_ready("s-4".into(), OLD).await.unwrap();

    // s-5: old unread existed but was READ → NOT stalled.
    store.record_agent_spawn(spawn_agent_row("s-5")).await.unwrap();
    store.record_shim_ready("s-5".into(), OLD).await.unwrap();
    let m = store.insert_message(mail("s-5", OLD)).await.unwrap();
    store.mark_read(vec![m.id], "s-5".into(), NOW).await.unwrap();

    let stalled = store
        .stalled_agents_with_unread(
            vec!["s-1".into(), "s-2".into(), "s-3".into(), "s-4".into(), "s-5".into()],
            NOW,
            SILENCE,
        )
        .await
        .unwrap();
    assert!(
        stalled.contains("s-1"),
        "alive + old unread + silent must flag"
    );
    assert!(!stalled.contains("s-2"), "recent activity must clear it");
    assert!(!stalled.contains("s-3"), "fresh unread must not flag");
    assert!(!stalled.contains("s-4"), "no unread must not flag");
    assert!(!stalled.contains("s-5"), "read mail must not flag");
    assert_eq!(stalled.len(), 1);
}

#[tokio::test]
async fn stalled_agents_with_unread_never_flags_the_dead_or_unborn() {
    const NOW: i64 = 1_700_001_000_000;
    const SILENCE: i64 = 600_000;
    const OLD: i64 = 1_700_000_300_000;
    let (_dir, store) = fresh_store().await;
    let mail = |to: &str| NewMessage {
        from_agent: "orchestrator".into(),
        to_agent: to.into(),
        kind: "note".into(),
        body: "ping".into(),
        sent_at: OLD,
        in_reply_to: None,
        meta: None,
    };

    // s-6: killed after the mail arrived → NOT stalled (dead agents are the
    // handoff_missing / error lanes' business, never "stuck").
    store.record_agent_spawn(spawn_agent_row("s-6")).await.unwrap();
    store.record_shim_ready("s-6".into(), OLD).await.unwrap();
    store.insert_message(mail("s-6")).await.unwrap();
    store.record_agent_kill("s-6".into(), NOW).await.unwrap();

    // s-7: shim exited → NOT stalled, same reasoning.
    store.record_agent_spawn(spawn_agent_row("s-7")).await.unwrap();
    store.record_shim_ready("s-7".into(), OLD).await.unwrap();
    store.insert_message(mail("s-7")).await.unwrap();
    store.record_shim_exit("s-7".into(), 0, NOW).await.unwrap();

    // s-8: never shim-ready (still starting) → NOT stalled; the
    // first-response watchdog owns that window.
    store.record_agent_spawn(spawn_agent_row("s-8")).await.unwrap();
    store.insert_message(mail("s-8")).await.unwrap();

    let stalled = store
        .stalled_agents_with_unread(
            vec![
                "s-6".into(),
                "s-7".into(),
                "s-8".into(),
                "no-such-agent".into(),
            ],
            NOW,
            SILENCE,
        )
        .await
        .unwrap();
    assert!(stalled.is_empty(), "dead/starting/unknown must never flag");

    // Empty input short-circuits without touching the db.
    assert!(
        store
            .stalled_agents_with_unread(Vec::new(), NOW, SILENCE)
            .await
            .unwrap()
            .is_empty()
    );
}

// ── dangling in_reply_to is dropped, not FK-500'd (QA 2026-06-21) ──────────
// An LLM worker can set `in_reply_to` to a message id that doesn't exist
// (hallucinated, cross-thread, or not-yet-committed). The column is
// `REFERENCES messages(id)`, so a raw insert fails the FK constraint and 500s
// the whole send — losing the agent's reply (live-observed: an opencode worker
// got four 500s before a retry landed). insert_message_threaded must sanitize a
// missing parent to NULL so the message still delivers, unthreaded.
#[tokio::test]
async fn dangling_in_reply_to_is_dropped_not_errored() {
    let (_dir, store) = fresh_store().await;
    let parent = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "user".into(),
                to_agent: "claude-aaaa".into(),
                kind: "task".into(),
                body: "parent".into(),
                sent_at: ts(1),
                in_reply_to: None,
                meta: None,
            },
            Some("t-1".into()),
        )
        .await
        .unwrap();

    // A valid reply keeps its linkage.
    let valid = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "claude-aaaa".into(),
                to_agent: "user".into(),
                kind: "reply".into(),
                body: "valid reply".into(),
                sent_at: ts(2),
                in_reply_to: Some(parent.id),
                meta: None,
            },
            Some("t-1".into()),
        )
        .await
        .expect("valid in_reply_to must insert");
    assert_eq!(valid.in_reply_to, Some(parent.id), "valid parent ref preserved");

    // A dangling reply must NOT error, and the bad ref is dropped to NULL.
    let dangling = store
        .insert_message_threaded(
            NewMessage {
                from_agent: "claude-aaaa".into(),
                to_agent: "user".into(),
                kind: "reply".into(),
                body: "reply to a ghost".into(),
                sent_at: ts(3),
                in_reply_to: Some(999_999),
                meta: None,
            },
            Some("t-1".into()),
        )
        .await
        .expect("dangling in_reply_to must NOT fail the insert");
    assert_eq!(
        dangling.in_reply_to, None,
        "non-existent parent ref must be dropped to NULL, not persisted"
    );

    // The dangling-ref message is actually delivered, not silently lost.
    let all = store
        .list_messages(ListMessagesOpts {
            to_agent: Some("user".into()),
            from_agent: None,
            thread_id: None,
            only_undelivered: false,
            limit: 200,
        })
        .await
        .unwrap();
    assert!(
        all.iter()
            .any(|m| m.id == dangling.id && m.in_reply_to.is_none()),
        "the dangling-ref message must be delivered with a NULL parent"
    );
}
