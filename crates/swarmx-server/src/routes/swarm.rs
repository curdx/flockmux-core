//! Swarm REST: `/api/message`, `/api/blackboard`, `/api/blackboard/*path`.

use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use swarmx_protocol::rest::{
    BlackboardEntry, BlackboardHistoryEntry, BlackboardSnapshot, ConsumeWakesResponse,
    MarkReadRequest, MarkReadResponse, MessageRecord, SendMessageRequest, ThoughtTrace,
    ThoughtTraceStep, WriteBlackboardRequest,
};
use swarmx_storage::{ListMessagesOpts, ThoughtTraceRecord as StoreThoughtTraceRecord};
use swarmx_swarm::{path_safe, NewMessage, SwarmEvent};

#[derive(Debug, Deserialize, Default)]
pub struct ListMessagesQuery {
    pub to: Option<String>,
    pub from: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
    /// Scope history to one direction (thread) so a quiet thread's older
    /// messages aren't pushed out of the global `limit` window. P1-04.
    pub thread_id: Option<String>,
    #[serde(default)]
    pub only_undelivered: bool,
}

pub async fn list_messages(
    State(state): State<AppState>,
    Query(q): Query<ListMessagesQuery>,
) -> Result<Json<Vec<MessageRecord>>, (StatusCode, Json<serde_json::Value>)> {
    let items = if let Some(query) = q.q {
        let mut items = state
            .store
            .search_messages(query)
            .await
            .map_err(internal_err)?;
        // The search branch used to silently drop `?limit=`: `search_messages`
        // takes no limit param (its SQL hard-caps at 200), so the cap is applied
        // here at the route layer. Mirrors the non-search branch's default and
        // ListMessagesOpts' `<=0 → 200` clamp.
        items.truncate(effective_search_limit(q.limit));
        items
    } else {
        state
            .store
            .list_messages(ListMessagesOpts {
                to_agent: q.to,
                from_agent: q.from,
                thread_id: q.thread_id,
                only_undelivered: q.only_undelivered,
                limit: q.limit.unwrap_or(200),
            })
            .await
            .map_err(internal_err)?
    };
    Ok(Json(
        items
            .into_iter()
            .map(|r| MessageRecord {
                id: r.id,
                from_agent: r.from_agent,
                to_agent: r.to_agent,
                kind: r.kind,
                body: r.body,
                sent_at: r.sent_at,
                delivered_at: r.delivered_at,
                read_at: r.read_at,
                in_reply_to: r.in_reply_to,
                thread_id: r.thread_id,
                meta: r.meta,
                thought_trace: r.thought_trace.as_ref().map(storage_trace_to_rest),
            })
            .collect(),
    ))
}

/// Short-window idempotency for `/api/message`. An MCP `swarm_send_message` that
/// SUCCEEDED server-side but whose response the agent missed (client timeout,
/// dropped connection) is retried by the LLM with byte-identical content — which
/// would otherwise insert a duplicate the recipient reads and may double-reply
/// to. We key on a content fingerprint and return the FIRST result within a
/// window that comfortably exceeds the 30s mutating-client timeout (so a retry
/// fired after that timeout still lands inside it). LLM retries are SERIAL (one
/// tool call at a time), so a check-then-insert without holding a lock across
/// the DB write suffices: a truly concurrent same-fingerprint POST would be two
/// independent legitimate messages, not a retry.
const MSG_DEDUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

type MsgDedup =
    std::sync::Mutex<std::collections::HashMap<u64, (MessageRecord, std::time::Instant)>>;

fn msg_dedup() -> &'static MsgDedup {
    static D: std::sync::OnceLock<MsgDedup> = std::sync::OnceLock::new();
    D.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn msg_fingerprint(from: &str, req: &SendMessageRequest) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    from.hash(&mut h);
    req.to.hash(&mut h);
    req.kind.hash(&mut h);
    req.body.hash(&mut h);
    req.in_reply_to.hash(&mut h);
    h.finish()
}

pub async fn send_message(
    State(state): State<AppState>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<MessageRecord>, (StatusCode, Json<serde_json::Value>)> {
    let from = req.from.clone().unwrap_or_else(|| "system".into());
    let fp = msg_fingerprint(&from, &req);
    // Idempotency: a retried identical send within the window returns the first
    // result instead of inserting a duplicate (and does NOT re-fire the auto-wake).
    {
        let mut d = msg_dedup().lock().unwrap_or_else(|e| e.into_inner());
        d.retain(|_, (_, t)| t.elapsed() < MSG_DEDUP_WINDOW);
        if let Some((rec, _)) = d.get(&fp) {
            tracing::debug!(from = %from, to = %rec.to_agent, id = rec.id, "send_message idempotent hit (identical retry within window)");
            return Ok(Json(rec.clone()));
        }
    }
    let record = state
        .swarm
        .send_message(NewMessage {
            from_agent: from,
            to_agent: req.to,
            kind: req.kind,
            body: req.body,
            sent_at: now_ms(),
            in_reply_to: req.in_reply_to,
            // Agent / user free-text via REST carries no server-stamped
            // structure; the UI classifies these with its body heuristics.
            meta: None,
        })
        .await
        .map_err(internal_err)?;

    let out = MessageRecord {
        id: record.id,
        from_agent: record.from_agent,
        to_agent: record.to_agent,
        kind: record.kind,
        body: record.body,
        sent_at: record.sent_at,
        delivered_at: record.delivered_at,
        read_at: record.read_at,
        in_reply_to: record.in_reply_to,
        thread_id: record.thread_id,
        meta: record.meta,
        thought_trace: record.thought_trace.as_ref().map(storage_trace_to_rest),
    };
    // Cache BEFORE the auto-wake so a fast retry can't slip in during the ~150ms
    // wake spawn and double-insert.
    msg_dedup()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(fp, (out.clone(), std::time::Instant::now()));

    // W0-2: close the "external message doesn't wake the recipient" gap.
    // `/api/message` is the entry point for the UI, scripts, and the future
    // public API — but only the UI used to follow up with a manual wake, so a
    // bare POST left the orchestrator asleep on a fresh instruction. Auto-wake
    // the recipient when an EXTERNAL sender (user/system) messages a LIVE agent.
    // Agent-to-agent traffic (from = an agent id) is deliberately excluded — it
    // is driven by the BlackboardChanged wake path and must not double-kick.
    // cron uses the core `swarm.send_message` (not this handler) + its own wake,
    // so it's unaffected. Fire-and-forget so the response isn't held on the
    // ~150ms PTY settle inside deliver_manual_wake.
    {
        let to = out.to_agent.clone();
        let external = matches!(out.from_agent.as_str(), "user" | "system");
        if external && to != "user" && state.registry.get(&to).is_some() {
            let swarm = state.swarm.clone();
            let registry = state.registry.clone();
            let server_url = state.server_url.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    crate::wake::deliver_manual_wake(&swarm, &registry, &server_url, &to).await
                {
                    tracing::debug!(?e, agent = %to, "auto-wake on send_message failed (best-effort)");
                }
            });
        }
    }

    Ok(Json(out))
}

/// One structured event from the web UI's debug logger (`web/src/lib/debugLog.ts`).
/// We don't model the payload — it's free-form diagnostic context per `ev`.
#[derive(Debug, Deserialize)]
pub struct WebDebugEvent {
    /// Client-side wall clock (ms) when the event happened.
    pub ts: Option<f64>,
    /// Per-page monotonic counter so ordering survives same-ms events.
    pub seq: Option<u64>,
    /// Short event tag, e.g. "send.start", "refresh.replace", "live.append".
    pub ev: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct WebDebugBatch {
    pub events: Vec<WebDebugEvent>,
}

/// `POST /api/debug/log` — sink for the web UI's chat-lifecycle breadcrumbs.
///
/// The whole point: front-end events land in the SAME `tracing` stream (and so
/// the same `~/.swarmx/logs/swarmx.log` file) as the backend's, interleaved
/// in arrival order. That gives one timeline for "user sent → reached backend →
/// stored → broadcast → echoed back → rendered", so a dropped message shows
/// exactly which hop lost it. Logged on the `swarmx::web` target at INFO so it
/// always survives the default `info,swarmx=debug` filter.
pub async fn web_debug_log(
    State(_state): State<AppState>,
    Json(batch): Json<WebDebugBatch>,
) -> StatusCode {
    for e in batch.events {
        tracing::info!(
            target: "swarmx::web",
            seq = e.seq.unwrap_or(0),
            client_ts = e.ts.unwrap_or(0.0),
            ev = %e.ev,
            data = %e.data,
            "WEB",
        );
    }
    StatusCode::NO_CONTENT
}

fn storage_trace_to_rest(trace: &StoreThoughtTraceRecord) -> ThoughtTrace {
    let summary =
        serde_json::from_str::<Vec<swarmx_storage::ThoughtTraceStep>>(&trace.summary_json)
            .unwrap_or_default()
            .into_iter()
            .map(|s| ThoughtTraceStep {
                phase: s.phase,
                label: s.label,
                source: s.source,
                at: s.at,
            })
            .collect();
    ThoughtTrace {
        id: trace.id.clone(),
        trigger_message_id: trace.trigger_message_id,
        response_message_id: trace.response_message_id,
        agent_id: trace.agent_id.clone(),
        workspace_id: trace.workspace_id.clone(),
        thread_id: trace.thread_id.clone(),
        status: trace.status.clone(),
        started_at: trace.started_at,
        completed_at: trace.completed_at,
        summary,
        updated_at: trace.updated_at,
    }
}

/// `POST /api/message/read` — caller declares which messages it has read.
/// The server filters by `to_agent` so cross-agent marks are silently
/// dropped (no error, just an empty `marked` list).
pub async fn mark_messages_read(
    State(state): State<AppState>,
    Json(req): Json<MarkReadRequest>,
) -> Result<Json<MarkReadResponse>, (StatusCode, Json<serde_json::Value>)> {
    let at = now_ms();
    let marked = state
        .swarm
        .mark_read(req.to, req.ids)
        .await
        .map_err(internal_err)?;
    Ok(Json(MarkReadResponse { marked, at }))
}

/// Shared `?to=<agent_id>` query for the consume-wakes endpoint. (Was also used
/// by the now-removed `unread_count` GET — that endpoint was dead: the web UI
/// never called it and `wake-check` switched to `consume_wakes`. See M6f below.)
#[derive(Debug, Deserialize)]
pub struct UnreadCountQuery {
    pub to: String,
}

/// M6f: atomically claim all pending wakes for an agent.
///
/// Replaces `unread_count` as `wake_check`'s primary signal. Returns the
/// ids of `kind="wake"` messages that were unread before this call AND
/// have now been marked read. If the list is non-empty, `wake_check`
/// should emit `block` with a reason that lists those wakes.
///
/// Why a dedicated endpoint vs reusing `unread_count` + `mark_read`:
///   - **Atomicity**: this collapses "see if there are wakes" and "mark
///     them read" into one SQL transaction. The two-call alternative
///     opens a window where a wake arriving between SELECT and UPDATE
///     would be marked-read without being delivered to `wake_check`.
///   - **Semantic clarity**: wake messages aren't human mail. They're
///     consumed by the Stop hook. Having a dedicated verb keeps that
///     distinction visible in the routes table.
///   - **Bug source for M6f**: the previous design relied on
///     `swarm_list_messages` (called by the LLM) marking wakes read.
///     During long turns the LLM would mid-turn-list and silently mark
///     a freshly-arrived wake read before `wake_check` ever saw it,
///     stranding the agent until manual ⚡ wake. Observed in 2026-05-23
///     strict e2e #6.
///
/// B1: the response also inlines each consumed wake's payload (the mailbox
/// note plus a snapshot of the blackboard key it points at). The rows were
/// already claimed by the atomic UPDATE above, so the extra read is nearly
/// free — and it lets `wake-check` hand the LLM a content digest directly
/// instead of the old "you have N wakes, go list" reason, which cost the
/// woken captain 3–5 serial LLM round-trips (`swarm_list_messages` →
/// `swarm_read_blackboard` → …) before it could speak. Enrichment is
/// fail-soft: any gap degrades to the old count-only contract, never a 500.
pub async fn consume_wakes(
    State(state): State<AppState>,
    Query(q): Query<UnreadCountQuery>,
) -> Result<Json<ConsumeWakesResponse>, (StatusCode, Json<serde_json::Value>)> {
    let at = now_ms();
    let resp = crate::wake_claim::claim(&state.swarm, &q.to, at)
        .await
        .map_err(internal_err)?;
    Ok(Json(resp))
}

/// Effective row cap for the search branch of `list_messages`:
/// `search_messages` has no limit param (hard `LIMIT 200` in SQL), so the
/// route truncates after the fact. Mirrors the non-search branch's
/// `q.limit.unwrap_or(200)` and the store's `<=0 → 200` clamp.
fn effective_search_limit(limit: Option<i64>) -> usize {
    match limit {
        Some(l) if l > 0 => l as usize,
        _ => 200,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct BlackboardHistoryQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub include_content: Option<bool>,
}

pub async fn blackboard_history(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(opts): Query<BlackboardHistoryQuery>,
) -> Result<Json<Vec<BlackboardHistoryEntry>>, (StatusCode, Json<serde_json::Value>)> {
    let ops = state
        .store
        .list_blackboard_ops(Some(path))
        .await
        .map_err(internal_err)?;
    let include_content = opts.include_content.unwrap_or(false);
    let limit = opts.limit.unwrap_or(50).max(1) as usize;
    Ok(Json(
        ops.into_iter()
            .take(limit)
            .map(|r| BlackboardHistoryEntry {
                id: r.id,
                agent_id: r.agent_id,
                op: r.op,
                path: r.path,
                sha256: r.sha256,
                at: r.at,
                content: if include_content {
                    Some(r.content)
                } else {
                    None
                },
            })
            .collect(),
    ))
}

/// Query for `GET /api/blackboard`. `scope` optionally restricts the listing
/// to one direction's `<workspace_id>/<thread_slug>` key prefix. Omitted = the
/// historical global listing (every path), which is correct for the
/// collaborative model where a direction's workers share a prefix and are meant
/// to see each other's keys. A fusion competition passes its contestant
/// direction's prefix so the contestants can't see each other's blackboard.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ListBlackboardQuery {
    pub scope: Option<String>,
}

pub async fn list_blackboard_paths(
    State(state): State<AppState>,
    Query(q): Query<ListBlackboardQuery>,
) -> Result<Json<Vec<BlackboardEntry>>, (StatusCode, Json<serde_json::Value>)> {
    let latest = state
        .store
        .list_blackboard_ops_scoped(q.scope)
        .await
        .map_err(internal_err)?;
    Ok(Json(
        latest
            .into_iter()
            // Hide paths whose latest op is a `delete` tombstone: the file is
            // gone from disk, so listing it would resurrect a ghost the user
            // can't open. The op-log row is kept (history stays truthful) —
            // `blackboard_history` still shows the delete.
            .filter(|r| r.op != "delete")
            .map(|r| BlackboardEntry {
                path: r.path,
                sha256: r.sha256,
                at: r.at,
                op: r.op,
            })
            .collect(),
    ))
}

pub async fn read_blackboard(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Result<Json<BlackboardSnapshot>, (StatusCode, Json<serde_json::Value>)> {
    let content = state
        .swarm
        .read_blackboard(&path)
        .await
        .map_err(bad_request_err)?;
    let content = match content {
        Some(c) => c,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("blackboard path not found: {path}")})),
            ))
        }
    };
    // Find the latest op for this path so we can return sha + at.
    let ops = state
        .store
        .list_blackboard_ops(Some(path.clone()))
        .await
        .map_err(internal_err)?;
    let (sha, at) = ops
        .first()
        .map(|r| (r.sha256.clone(), r.at))
        .unwrap_or_else(|| (sha256_hex(content.as_bytes()), 0));
    Ok(Json(BlackboardSnapshot {
        path,
        content,
        sha256: sha,
        at,
    }))
}

pub async fn write_blackboard(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Json(req): Json<WriteBlackboardRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state
        .swarm
        .write_blackboard(req.agent_id, &path, &req.content)
        .await
        .map_err(bad_request_err)?;
    Ok(Json(json!({
        "id": record.id,
        "path": record.path,
        "sha256": record.sha256,
        "at": record.at,
    })))
}

/// `DELETE /api/blackboard/*path` — remove a single blackboard file and
/// record a `delete` tombstone op so history stays truthful.
///
/// Path safety: this reuses the SAME jail the read handler uses
/// (`path_safe::resolve_existing` against the swarm's blackboard root) — no
/// weaker check. A missing/escaping path is rejected with 400 (consistent with
/// `read_blackboard`'s `bad_request_err`), and a path that resolves outside the
/// root can never reach `fs::remove_file`.
pub async fn delete_blackboard(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let root = state.swarm.blackboard_root().to_path_buf();
    let target = path_safe::resolve_existing(&root, &path).map_err(bad_request_err)?;

    // Remove the file off the runtime thread (same as the write path's fs work).
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        match std::fs::remove_file(&target) {
            Ok(()) => Ok(()),
            // Already gone on disk is fine — we still want to record the
            // tombstone + broadcast so the op-log and UI converge.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    })
    .await
    .map_err(|e| internal_err(anyhow::anyhow!("spawn_blocking remove_file: {e}")))?
    .map_err(|e| internal_err(anyhow::anyhow!("remove blackboard file: {e}")))?;

    let at = now_ms();
    // Record the delete op (history). A failed op-log insert must NOT swallow
    // the broadcast — the file IS gone, so dependents/UI still need to converge.
    // Mirror write_blackboard's posture: log, broadcast with id=-1, return Ok.
    let id = match state
        .store
        .record_blackboard_delete(None, path.clone(), at)
        .await
    {
        Ok(record) => record.id,
        Err(e) => {
            tracing::warn!(
                ?e,
                path = %path,
                "blackboard delete op-log insert failed; file IS removed — broadcasting anyway (id=-1)"
            );
            -1
        }
    };
    state.swarm.publish_event(SwarmEvent::BlackboardChanged {
        id,
        agent_id: None,
        op: "delete".into(),
        path: path.clone(),
        sha256: String::new(),
        at,
    });
    Ok(Json(json!({ "ok": true, "path": path, "at": at })))
}

fn internal_err(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    tracing::warn!(?e, "swarm route error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": e.to_string()})),
    )
}

fn bad_request_err(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    tracing::debug!(?e, "swarm route bad request");
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": e.to_string()})),
    )
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(to: &str, kind: &str, body: &str, reply: Option<i64>) -> SendMessageRequest {
        SendMessageRequest {
            from: None,
            to: to.into(),
            kind: kind.into(),
            body: body.into(),
            in_reply_to: reply,
        }
    }

    #[test]
    fn fingerprint_stable_for_identical_send_and_varies_on_any_field() {
        let a = req("orch", "reply", "done", Some(3));
        // Same sender + same content → same key: a byte-identical LLM retry dedups.
        assert_eq!(
            msg_fingerprint("worker-1", &a),
            msg_fingerprint("worker-1", &req("orch", "reply", "done", Some(3)))
        );
        // Any change → different key: a genuinely different message is NOT deduped.
        let base = msg_fingerprint("worker-1", &a);
        assert_ne!(base, msg_fingerprint("worker-2", &a), "sender");
        assert_ne!(
            base,
            msg_fingerprint("worker-1", &req("orch2", "reply", "done", Some(3))),
            "to"
        );
        assert_ne!(
            base,
            msg_fingerprint("worker-1", &req("orch", "chat", "done", Some(3))),
            "kind"
        );
        assert_ne!(
            base,
            msg_fingerprint("worker-1", &req("orch", "reply", "done2", Some(3))),
            "body"
        );
        assert_ne!(
            base,
            msg_fingerprint("worker-1", &req("orch", "reply", "done", None)),
            "in_reply_to"
        );
    }

    #[test]
    fn search_limit_defaults_and_clamps() {
        assert_eq!(effective_search_limit(None), 200);
        assert_eq!(effective_search_limit(Some(5)), 5);
        assert_eq!(effective_search_limit(Some(0)), 200);
        assert_eq!(effective_search_limit(Some(-3)), 200);
    }
}
