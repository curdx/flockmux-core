//! Swarm REST: `/api/message`, `/api/blackboard`, `/api/blackboard/*path`.

use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use swarmx_protocol::rest::{
    BlackboardEntry, BlackboardHistoryEntry, BlackboardSnapshot, ConsumeWakeItem,
    ConsumeWakesResponse, MarkReadRequest, MarkReadResponse, MessageRecord, SendMessageRequest,
    ThoughtTrace, ThoughtTraceStep, WriteBlackboardRequest,
};
use swarmx_storage::{ListMessagesOpts, ThoughtTraceRecord as StoreThoughtTraceRecord};
use swarmx_swarm::{path_safe, NewMessage, SwarmEvent};
use serde::Deserialize;
use serde_json::json;

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

type MsgDedup = std::sync::Mutex<std::collections::HashMap<u64, (MessageRecord, std::time::Instant)>>;

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
                if let Err(e) = crate::wake::deliver_manual_wake(&swarm, &registry, &server_url, &to).await {
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
    let ids = state
        .store
        .consume_wakes(q.to.clone(), at)
        .await
        .map_err(internal_err)?;
    // Broadcast message_read so the UI badge updates promptly. Match
    // the shape that mark_messages_read emits — same event kind, same
    // ids field — so the FE doesn't need a new handler.
    if !ids.is_empty() {
        use swarmx_protocol::ws_swarm::SwarmEvent;
        state.swarm.publish_event(SwarmEvent::MessageRead {
            ids: ids.clone(),
            to_agent: q.to.clone(),
            at,
        });
    }
    let wakes = inline_wake_payloads(&state.swarm, &q.to, &ids).await;
    Ok(Json(ConsumeWakesResponse {
        to: q.to,
        count: ids.len() as i64,
        ids,
        wakes,
    }))
}

/// Per-entry inline payload cap (bytes). 64KB keeps one pathological
/// blackboard write from blowing up the Stop-hook continuation prompt; the
/// `truncated` flag tells the LLM to `swarm_read_blackboard` for the rest.
const WAKE_PAYLOAD_ENTRY_CAP: usize = 64 * 1024;
/// Total inline-content budget per response (bytes). Wakes are claimed in a
/// batch after a long idle stretch, so cap the aggregate to keep the response
/// — and the prompt built from it — sane. Fail-soft: entries past the budget
/// carry `content: None`, never an error.
const WAKE_PAYLOAD_TOTAL_CAP: usize = 256 * 1024;
/// Max consumed wakes inlined per response. `ids`/`count` always reflect the
/// full consumed set; only this inline array is bounded.
const WAKE_PAYLOAD_MAX_ITEMS: usize = 50;

/// Build the `wakes` inline array for `consume_wakes`. Pure enrichment over
/// rows that are ALREADY marked read: any failure (page miss, vanished key,
/// store error) just yields fewer/no items — the count-only contract still
/// holds, so nothing here may propagate an error.
async fn inline_wake_payloads(
    swarm: &swarmx_swarm::Swarm,
    to: &str,
    ids: &[i64],
) -> Vec<ConsumeWakeItem> {
    if ids.is_empty() {
        return Vec::new();
    }
    // Fetch the just-consumed rows back. `consume_wakes` already marked them
    // read, but `list_messages` has no read filter, so one recent-page pull
    // re-finds them. A consumed id that somehow fell out of the page
    // (pathological backlog) simply gets no inline payload — fail-soft.
    let records = match swarm
        .store()
        .list_messages(ListMessagesOpts {
            to_agent: Some(to.to_string()),
            from_agent: None,
            thread_id: None,
            only_undelivered: false,
            limit: 200,
        })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(?e, to, "consume_wakes payload enrichment failed; count-only response");
            return Vec::new();
        }
    };
    let mut by_id: std::collections::HashMap<i64, _> = records
        .into_iter()
        .filter(|r| r.kind == "wake")
        .map(|r| (r.id, r))
        .collect();
    // Oldest-first, so the LLM reads the digests in arrival order.
    let mut ordered: Vec<i64> = ids.to_vec();
    ordered.sort_unstable();
    let mut items = Vec::new();
    let mut budget = WAKE_PAYLOAD_TOTAL_CAP;
    for id in ordered.into_iter().take(WAKE_PAYLOAD_MAX_ITEMS) {
        let Some(rec) = by_id.remove(&id) else {
            continue;
        };
        let key = rec
            .meta
            .as_ref()
            .and_then(|m| m.get("key"))
            .and_then(|k| k.as_str())
            .map(str::to_string);
        let (body, mut truncated) = truncate_utf8(rec.body, WAKE_PAYLOAD_ENTRY_CAP);
        let mut content = None;
        if let Some(k) = &key {
            if budget > 0 {
                // Missing/unreadable key (deleted after the wake fired) is a
                // None, not an error — the LLM still has the note body.
                if let Ok(Some(c)) = swarm.read_blackboard(k).await {
                    let (c, cut) = truncate_utf8(c, WAKE_PAYLOAD_ENTRY_CAP.min(budget));
                    budget = budget.saturating_sub(c.len());
                    truncated = truncated || cut;
                    content = Some(c);
                }
            }
        }
        items.push(ConsumeWakeItem {
            id,
            from_agent: rec.from_agent,
            sent_at: rec.sent_at,
            body,
            key,
            content,
            truncated,
        });
    }
    items
}

/// Cut `s` to at most `cap` bytes on a char boundary. Returns `(cut, true)`
/// when anything was dropped. The continuation prompt this feeds is read by
/// an LLM, so a mid-char splice would be silently mojibake — never split
/// inside a UTF-8 sequence.
fn truncate_utf8(s: String, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s, false);
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
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
        assert_ne!(base, msg_fingerprint("worker-1", &req("orch2", "reply", "done", Some(3))), "to");
        assert_ne!(base, msg_fingerprint("worker-1", &req("orch", "chat", "done", Some(3))), "kind");
        assert_ne!(base, msg_fingerprint("worker-1", &req("orch", "reply", "done2", Some(3))), "body");
        assert_ne!(base, msg_fingerprint("worker-1", &req("orch", "reply", "done", None)), "in_reply_to");
    }

    // ── B1: consume_wakes inline payload ─────────────────────────────────

    async fn fresh_swarm() -> (tempfile::TempDir, std::sync::Arc<swarmx_swarm::Swarm>) {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("swarmx.db");
        let bb_root = dir.path().join("blackboard");
        std::fs::create_dir_all(&bb_root).unwrap();
        let store = std::sync::Arc::new(swarmx_storage::Store::open(&db_path).await.unwrap());
        (dir, swarmx_swarm::Swarm::new(store, bb_root))
    }

    /// Mirror of wake.rs::write_wake_mailbox's note shape (body + meta.key).
    async fn send_wake(swarm: &swarmx_swarm::Swarm, to: &str, key: Option<&str>) -> i64 {
        let meta = key
            .map(|k| serde_json::json!({"subtype": "wake", "reason": "blackboard", "key": k}));
        let rec = swarm
            .send_message(NewMessage {
                from_agent: "system".into(),
                to_agent: to.into(),
                kind: "wake".into(),
                body: match key {
                    Some(k) => format!("共享区 `{k}` 有更新，请查看"),
                    None => "操作员唤醒——请先查收邮箱里的新消息".into(),
                },
                sent_at: 1,
                in_reply_to: None,
                meta,
            })
            .await
            .unwrap();
        rec.id
    }

    #[tokio::test]
    async fn inline_payload_carries_blackboard_snapshot() {
        let (_dir, swarm) = fresh_swarm().await;
        swarm
            .write_blackboard(Some("worker-1".into()), "design.md", "# Design\nhello")
            .await
            .unwrap();
        let id = send_wake(&swarm, "cap", Some("design.md")).await;
        let ids = swarm.store().consume_wakes("cap".into(), 2).await.unwrap();
        assert_eq!(ids, vec![id]);

        let items = inline_wake_payloads(&swarm, "cap", &ids).await;
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.id, id);
        assert_eq!(it.from_agent, "system");
        assert_eq!(it.key.as_deref(), Some("design.md"));
        assert_eq!(it.content.as_deref(), Some("# Design\nhello"));
        assert!(it.body.contains("design.md"));
        assert!(!it.truncated);
    }

    #[tokio::test]
    async fn inline_payload_truncates_oversized_entry() {
        let (_dir, swarm) = fresh_swarm().await;
        let big = "a".repeat(70 * 1024);
        swarm
            .write_blackboard(Some("w".into()), "big.md", &big)
            .await
            .unwrap();
        send_wake(&swarm, "cap", Some("big.md")).await;
        let ids = swarm.store().consume_wakes("cap".into(), 2).await.unwrap();

        let items = inline_wake_payloads(&swarm, "cap", &ids).await;
        assert_eq!(items.len(), 1);
        let content = items[0].content.as_deref().unwrap();
        assert_eq!(content.len(), WAKE_PAYLOAD_ENTRY_CAP);
        assert!(items[0].truncated, "oversized entry must carry the truncated flag");
    }

    #[tokio::test]
    async fn inline_payload_total_budget_is_fail_soft() {
        let (_dir, swarm) = fresh_swarm().await;
        // 7 × 40KB = 280KB > 256KB budget: the first 6 fit whole, the 7th is
        // cut to the remaining 16KB — and nothing errors out.
        let chunk = "x".repeat(40 * 1024);
        for i in 0..7 {
            let key = format!("k{i}.md");
            swarm
                .write_blackboard(Some("w".into()), &key, &chunk)
                .await
                .unwrap();
            send_wake(&swarm, "cap", Some(&key)).await;
        }
        let ids = swarm.store().consume_wakes("cap".into(), 2).await.unwrap();
        assert_eq!(ids.len(), 7);

        let items = inline_wake_payloads(&swarm, "cap", &ids).await;
        assert_eq!(items.len(), 7);
        for it in &items[..6] {
            assert_eq!(it.content.as_deref().unwrap().len(), 40 * 1024);
            assert!(!it.truncated);
        }
        let last = &items[6];
        assert_eq!(
            last.content.as_deref().unwrap().len(),
            WAKE_PAYLOAD_TOTAL_CAP - 6 * 40 * 1024
        );
        assert!(last.truncated);
    }

    #[tokio::test]
    async fn inline_payload_missing_key_degrades_to_none() {
        let (_dir, swarm) = fresh_swarm().await;
        // Wake points at a key that was never written (or deleted since).
        send_wake(&swarm, "cap", Some("gone.md")).await;
        let ids = swarm.store().consume_wakes("cap".into(), 2).await.unwrap();

        let items = inline_wake_payloads(&swarm, "cap", &ids).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key.as_deref(), Some("gone.md"));
        assert_eq!(items[0].content, None, "missing key is fail-soft, not an error");
        assert!(!items[0].truncated);
        assert!(items[0].body.contains("gone.md"));
    }

    #[tokio::test]
    async fn inline_payload_caps_item_count() {
        let (_dir, swarm) = fresh_swarm().await;
        for _ in 0..(WAKE_PAYLOAD_MAX_ITEMS + 5) {
            send_wake(&swarm, "cap", None).await;
        }
        let ids = swarm.store().consume_wakes("cap".into(), 2).await.unwrap();
        assert_eq!(ids.len(), WAKE_PAYLOAD_MAX_ITEMS + 5, "ids stay the full consumed set");

        let items = inline_wake_payloads(&swarm, "cap", &ids).await;
        assert_eq!(items.len(), WAKE_PAYLOAD_MAX_ITEMS);
        // Oldest-first ordering.
        assert!(items.windows(2).all(|w| w[0].id < w[1].id));
    }

    #[test]
    fn truncate_utf8_never_splits_a_char() {
        // '界' is 3 bytes; a cap landing mid-char must back off to the boundary.
        let (cut, truncated) = truncate_utf8("界".repeat(1000), 1001);
        assert!(truncated);
        assert_eq!(cut.len(), 999);
        let (full, truncated) = truncate_utf8("short".into(), 64);
        assert!(!truncated);
        assert_eq!(full, "short");
    }

    #[test]
    fn search_limit_defaults_and_clamps() {
        assert_eq!(effective_search_limit(None), 200);
        assert_eq!(effective_search_limit(Some(5)), 5);
        assert_eq!(effective_search_limit(Some(0)), 200);
        assert_eq!(effective_search_limit(Some(-3)), 200);
    }
}
