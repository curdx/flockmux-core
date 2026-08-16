//! Claim / peek pending mailbox wakes and attach the B1 continuation.
//!
//! HTTP `POST /api/message/consume_wakes`, PTY/opencode kicks, and the
//! reasonix/zulu HTTP clients all go through here (or through the HTTP
//! endpoint that wraps [`claim`]). The continuation string lives on
//! [`swarmx_protocol::rest::ConsumeWakesResponse`] — this module only
//! fills `ids` + `wakes`.

use anyhow::Result;
use swarmx_protocol::rest::{ConsumeWakeItem, ConsumeWakesResponse};
use swarmx_storage::ListMessagesOpts;
use swarmx_swarm::{Swarm, SwarmEvent};

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

/// Atomically claim pending wakes, enrich, fill `reason`, and broadcast
/// `MessageRead` so the UI badge updates. This is the HTTP endpoint and
/// the in-process post-kick consume.
pub async fn claim(swarm: &Swarm, to: &str, at_ms: i64) -> Result<ConsumeWakesResponse> {
    let ids = swarm.store().consume_wakes(to.to_string(), at_ms).await?;
    if !ids.is_empty() {
        swarm.publish_event(SwarmEvent::MessageRead {
            ids: ids.clone(),
            to_agent: to.to_string(),
            at: at_ms,
        });
    }
    let wakes = inline_wake_payloads(swarm, to, &ids).await;
    Ok(ConsumeWakesResponse::assemble(to.to_string(), ids, wakes))
}

/// Snapshot pending wakes **without** marking them read. PTY kicks peek
/// first so the injected continuation can carry the digest; consume still
/// happens after a successful inject (a failed kick must leave the mailbox
/// for the Stop hook).
pub async fn peek(swarm: &Swarm, to: &str) -> Result<ConsumeWakesResponse> {
    let ids = swarm.store().unread_wake_ids(to.to_string()).await?;
    let wakes = inline_wake_payloads(swarm, to, &ids).await;
    Ok(ConsumeWakesResponse::assemble(to.to_string(), ids, wakes))
}

/// Build the `wakes` inline array. Pure enrichment over rows that exist in
/// the recent page: any failure (page miss, vanished key, store error) just
/// yields fewer/no items — the count-only contract still holds, so nothing
/// here may propagate an error.
async fn inline_wake_payloads(swarm: &Swarm, to: &str, ids: &[i64]) -> Vec<ConsumeWakeItem> {
    if ids.is_empty() {
        return Vec::new();
    }
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
            tracing::debug!(
                ?e,
                to,
                "consume_wakes payload enrichment failed; count-only response"
            );
            return Vec::new();
        }
    };
    let mut by_id: std::collections::HashMap<i64, _> = records
        .into_iter()
        .filter(|r| r.kind == "wake")
        .map(|r| (r.id, r))
        .collect();
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
/// when anything was dropped.
fn truncate_utf8(s: String, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s, false);
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
        if end == 0 {
            break;
        }
    }
    (s[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmx_swarm::NewMessage;

    async fn fresh_swarm() -> (tempfile::TempDir, std::sync::Arc<Swarm>) {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("swarmx.db");
        let bb_root = dir.path().join("blackboard");
        std::fs::create_dir_all(&bb_root).unwrap();
        let store = std::sync::Arc::new(swarmx_storage::Store::open(&db_path).await.unwrap());
        (dir, Swarm::new(store, bb_root))
    }

    async fn send_wake(swarm: &Swarm, to: &str, key: Option<&str>) -> i64 {
        let meta =
            key.map(|k| serde_json::json!({"subtype": "wake", "reason": "blackboard", "key": k}));
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
        let resp = claim(&swarm, "cap", 2).await.unwrap();
        assert_eq!(resp.ids, vec![id]);
        assert_eq!(resp.wakes.len(), 1);
        let it = &resp.wakes[0];
        assert_eq!(it.id, id);
        assert_eq!(it.from_agent, "system");
        assert_eq!(it.key.as_deref(), Some("design.md"));
        assert_eq!(it.content.as_deref(), Some("# Design\nhello"));
        assert!(it.body.contains("design.md"));
        assert!(!it.truncated);
        assert!(
            resp.reason
                .contains("system wrote `design.md`: # Design\nhello"),
            "{}",
            resp.reason
        );
    }

    #[tokio::test]
    async fn peek_does_not_mark_read() {
        let (_dir, swarm) = fresh_swarm().await;
        swarm
            .write_blackboard(Some("w".into()), "k.md", "hi")
            .await
            .unwrap();
        let id = send_wake(&swarm, "cap", Some("k.md")).await;
        let peeked = peek(&swarm, "cap").await.unwrap();
        assert_eq!(peeked.ids, vec![id]);
        assert!(peeked.reason.contains("k.md"), "{}", peeked.reason);
        assert_eq!(swarm.store().count_unread("cap".into()).await.unwrap(), 1);
        let claimed = claim(&swarm, "cap", 3).await.unwrap();
        assert_eq!(claimed.ids, vec![id]);
        assert_eq!(swarm.store().count_unread("cap".into()).await.unwrap(), 0);
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
        let resp = claim(&swarm, "cap", 2).await.unwrap();
        assert_eq!(resp.wakes.len(), 1);
        let content = resp.wakes[0].content.as_deref().unwrap();
        assert_eq!(content.len(), WAKE_PAYLOAD_ENTRY_CAP);
        assert!(
            resp.wakes[0].truncated,
            "oversized entry must carry the truncated flag"
        );
    }

    #[tokio::test]
    async fn inline_payload_total_budget_is_fail_soft() {
        let (_dir, swarm) = fresh_swarm().await;
        let chunk = "x".repeat(40 * 1024);
        for i in 0..7 {
            let key = format!("k{i}.md");
            swarm
                .write_blackboard(Some("w".into()), &key, &chunk)
                .await
                .unwrap();
            send_wake(&swarm, "cap", Some(&key)).await;
        }
        let resp = claim(&swarm, "cap", 2).await.unwrap();
        assert_eq!(resp.ids.len(), 7);
        assert_eq!(resp.wakes.len(), 7);
        for it in &resp.wakes[..6] {
            assert_eq!(it.content.as_deref().unwrap().len(), 40 * 1024);
            assert!(!it.truncated);
        }
        let last = &resp.wakes[6];
        assert_eq!(
            last.content.as_deref().unwrap().len(),
            WAKE_PAYLOAD_TOTAL_CAP - 6 * 40 * 1024
        );
        assert!(last.truncated);
    }

    #[tokio::test]
    async fn inline_payload_missing_key_degrades_to_none() {
        let (_dir, swarm) = fresh_swarm().await;
        send_wake(&swarm, "cap", Some("gone.md")).await;
        let resp = claim(&swarm, "cap", 2).await.unwrap();
        assert_eq!(resp.wakes.len(), 1);
        assert_eq!(resp.wakes[0].key.as_deref(), Some("gone.md"));
        assert_eq!(
            resp.wakes[0].content, None,
            "missing key is fail-soft, not an error"
        );
        assert!(!resp.wakes[0].truncated);
        assert!(resp.wakes[0].body.contains("gone.md"));
    }

    #[tokio::test]
    async fn inline_payload_caps_item_count() {
        let (_dir, swarm) = fresh_swarm().await;
        for _ in 0..(WAKE_PAYLOAD_MAX_ITEMS + 5) {
            send_wake(&swarm, "cap", None).await;
        }
        let resp = claim(&swarm, "cap", 2).await.unwrap();
        assert_eq!(
            resp.ids.len(),
            WAKE_PAYLOAD_MAX_ITEMS + 5,
            "ids stay the full consumed set"
        );
        assert_eq!(resp.wakes.len(), WAKE_PAYLOAD_MAX_ITEMS);
        assert!(resp.wakes.windows(2).all(|w| w[0].id < w[1].id));
    }

    #[test]
    fn truncate_utf8_never_splits_a_char() {
        let (cut, truncated) = truncate_utf8("界".repeat(1000), 1001);
        assert!(truncated);
        assert_eq!(cut.len(), 999);
        let (full, truncated) = truncate_utf8("short".into(), 64);
        assert!(!truncated);
        assert_eq!(full, "short");
    }
}
