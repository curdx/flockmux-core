//! Drive a reasonix agent over its `reasonix serve` HTTP+SSE control API.
//!
//! reasonix has no TUI to type into and no opencode-style `/tui` side-channel;
//! instead its `serve` mode exposes a small HTTP API + an SSE event stream
//! (verified live against reasonix npm-v1.9.1):
//!   - `POST /submit {input}`            → start/continue a turn (202)
//!   - `POST /tool-approval-mode {mode}` → set "yolo" so tools auto-approve
//!   - `GET  /status`                    → {running, ...}
//!   - `GET  /events` (SSE)              → JSON events, one per `data:` line:
//!       turn_started · reasoning · text · message · tool_dispatch ·
//!       tool_progress · tool_result · usage · approval_request · ask_request ·
//!       notice · phase · compaction_* · turn_done
//!
//! swarmx runs ONE long-lived [`run_driver`] task per reasonix agent. It:
//!   1. waits for serve to bind, sets yolo, submits the bootstrap prompt;
//!   2. follows `/events`, and on **`turn_done`** consumes pending wakes and
//!      re-submits the wake reason (the turn-end → continue loop that the
//!      observe-only Stop hook can't do — same contract as the opencode plugin);
//!   3. mirrors `tool_dispatch`/`tool_result` into the Activity view via
//!      `POST /api/agent/:id/activity` (reasonix has no PTY/transcript to tail).
//!
//! Wakes that arrive while the agent is ALREADY idle (no turn running, so no
//! `turn_done` will fire) are delivered by the WakeCoordinator via
//! [`wake_if_idle`] instead. Both paths funnel through [`consume_and_submit`],
//! and `consume_wakes` is atomic, so at most one of them ever submits a turn.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use swarmx_protocol::rest::ConsumeWakesResponse;

use crate::registry::Registry;

/// Short control-call client (yolo / submit / status / consume / activity).
fn control_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reasonix control http client")
}

/// Streaming client for the long-lived `/events` SSE connection: NO total
/// timeout (the stream stays open for the agent's whole life).
fn stream_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .build()
        .context("build reasonix sse http client")
}

fn base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// POST `/submit {input}` — start or continue a turn. Returns Ok on 2xx.
pub async fn deliver_turn(port: u16, text: &str) -> Result<()> {
    let c = control_client()?;
    submit(&c, port, text).await
}

async fn submit(c: &reqwest::Client, port: u16, text: &str) -> Result<()> {
    let resp = c
        .post(format!("{}/submit", base(port)))
        .json(&json!({ "input": text }))
        .send()
        .await
        .context("reasonix /submit send")?;
    if !resp.status().is_success() {
        return Err(anyhow!("reasonix /submit HTTP {}", resp.status()));
    }
    Ok(())
}

/// Set tool-approval mode to "yolo" so the headless agent never parks on a tool
/// approval prompt. Best-effort; logged by the caller.
async fn set_yolo(c: &reqwest::Client, port: u16) -> Result<()> {
    let resp = c
        .post(format!("{}/tool-approval-mode", base(port)))
        .json(&json!({ "mode": "yolo" }))
        .send()
        .await
        .context("reasonix /tool-approval-mode send")?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "reasonix /tool-approval-mode HTTP {}",
            resp.status()
        ));
    }
    Ok(())
}

/// `GET /status` → `running` flag. `None` if serve is unreachable / malformed.
pub async fn is_running(port: u16) -> Option<bool> {
    let c = control_client().ok()?;
    let resp = c.get(format!("{}/status", base(port))).send().await.ok()?;
    let body = resp.json::<Value>().await.ok()?;
    body.get("running").and_then(|v| v.as_bool())
}

/// Outcome of a one-turn usability probe (see [`verify_one_turn`]).
pub enum TurnProbe {
    /// The model produced output → the engine really ran a turn.
    Ok,
    /// The turn ended / timed out with no model output (wedged, no quota, …).
    NoOutput,
    /// An auth/credential error surfaced in the event stream → needs login/key.
    Auth(String),
}

/// Verified one-turn usability check for the engine probe. reasonix isn't a TUI
/// and isn't keystroke-driven — the PTY carries only `serve`'s logs, never the
/// model's reply — so the probe drives the SAME HTTP+SSE control surface the live
/// driver uses: `POST /submit` a trivial turn, then follow `/events` and watch
/// for the model to actually PRODUCE output (a `text`/`message` event) before
/// `turn_done`. Launch-only can't tell a valid key from an invalid/expired one
/// (the `serve` binds either way and only 401s on the model call); this can.
/// `Ok` = produced output, `NoOutput` = none by the deadline, `Auth(detail)` =
/// an unauthorized/invalid-key event was seen.
pub async fn verify_one_turn(port: u16, prompt: &str, total: Duration) -> TurnProbe {
    // serve should already be up (the probe's `observe` waited for ShimReady),
    // but make sure it answers before we submit.
    if !wait_serve_ready(port, Duration::from_secs(10)).await {
        return TurnProbe::NoOutput;
    }
    if let Ok(c) = control_client() {
        let _ = set_yolo(&c, port).await;
    }

    // Open the SSE stream BEFORE submitting so a fast turn's `turn_done` can't
    // fire before we're listening.
    let Ok(sc) = stream_client() else {
        return TurnProbe::NoOutput;
    };
    let mut resp = match sc.get(format!("{}/events", base(port))).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return TurnProbe::NoOutput,
    };
    if deliver_turn(port, prompt).await.is_err() {
        return TurnProbe::NoOutput;
    }

    let start = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut produced = false;
    let mut auth: Option<String> = None;
    loop {
        let Some(remaining) = total.checked_sub(start.elapsed()) else {
            break;
        };
        // Bound each read by the remaining budget so a silent stream still exits.
        let chunk = match tokio::time::timeout(remaining, resp.chunk()).await {
            Ok(Ok(Some(c))) => c,
            // stream end, error, or deadline → stop and decide on what we saw.
            _ => break,
        };
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=nl).collect::<Vec<u8>>();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
            if !payload.starts_with('{') {
                continue;
            }
            // Auth/credential scan on the raw event text (a `notice`/error frame
            // carries the provider's message). Conservative needles only.
            if auth.is_none() {
                let low = payload.to_lowercase();
                if low.contains("unauthorized")
                    || low.contains("not logged in")
                    || low.contains("invalid api key")
                    || low.contains("invalid_api_key")
                    || low.contains("authentication")
                {
                    auth = Some(payload.chars().take(200).collect());
                }
            }
            let Ok(event) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            match event.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
                // Final model output (not `reasoning`, which can fire without a
                // real answer) — proof the model actually responded.
                "text" | "message" => produced = true,
                "turn_done" => {
                    if let Some(d) = auth {
                        return TurnProbe::Auth(d);
                    }
                    return if produced {
                        TurnProbe::Ok
                    } else {
                        TurnProbe::NoOutput
                    };
                }
                _ => {}
            }
        }
        if buf.len() > 1_048_576 {
            buf.clear();
        }
    }
    // Deadline / stream end without a `turn_done`.
    if let Some(d) = auth {
        TurnProbe::Auth(d)
    } else if produced {
        TurnProbe::Ok
    } else {
        TurnProbe::NoOutput
    }
}

/// Poll `GET /status` until serve answers (it binds within ~1s of spawn) or the
/// window elapses. Returns false if it never came up.
async fn wait_serve_ready(port: u16, overall: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < overall {
        if is_running(port).await.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// Atomically claim pending wakes for this agent and, if any, submit a fresh
/// turn. `submit_text = None` submits [`ConsumeWakesResponse::continuation`]
/// (the auto / turn_done path); `Some(text)` submits `text` verbatim (the
/// manual/operator path).
/// Returns true iff a turn was submitted. The atomic `consume_wakes` guarantees
/// only one caller ever sees count > 0.
pub async fn consume_and_submit(
    serve_port: u16,
    swarmx_url: &str,
    agent_id: &str,
    submit_text: Option<&str>,
) -> Result<bool> {
    let c = control_client()?;
    let resp = consume_wakes(&c, swarmx_url, agent_id).await?;
    if resp.count <= 0 {
        return Ok(false);
    }
    let text = submit_text
        .map(str::to_string)
        .unwrap_or_else(|| resp.continuation());
    submit(&c, serve_port, &text).await?;
    Ok(true)
}

/// Called by the WakeCoordinator for a reasonix agent: only deliver here when
/// the agent is IDLE (a mid-turn wake is picked up by the driver's `turn_done`
/// path instead). Avoids submitting a second turn on top of a running one.
/// `submit_text` is forwarded to [`consume_and_submit`] (None ⇒ generic recipe,
/// Some ⇒ operator body). A busy agent's wake falls to turn_done (generic
/// recipe) — acceptable for an agent already mid-turn.
pub async fn wake_if_idle(
    serve_port: u16,
    swarmx_url: &str,
    agent_id: &str,
    submit_text: Option<&str>,
) -> Result<bool> {
    match is_running(serve_port).await {
        // Busy: a mid-turn wake is picked up by the driver's `turn_done` path.
        Some(true) => Ok(false),
        // Idle: claim the wake and submit it as a fresh turn.
        Some(false) => consume_and_submit(serve_port, swarmx_url, agent_id, submit_text).await,
        // Serve unreachable (still booting, or transiently wedged): do NOT
        // consume. `consume_wakes` is atomic and would mark the mailbox wake
        // READ, but the follow-up `submit` against the unreachable port then
        // fails — NET-LOSING the wake (marked read, never delivered). This is
        // live-confirmed: freezing the serve process (SIGSTOP) made `read_at`
        // get stamped while `/submit send` failed in the same call. Leaving the
        // wake un-consumed in the mailbox lets it be re-delivered by the driver's
        // `turn_done` (once serve answers) or by the next BlackboardChanged. The
        // caller (`deliver_wake`) logs this Err as a `warn!`, doesn't panic.
        None => Err(anyhow!(
            "reasonix serve unreachable (status probe = None); leaving wake in \
             the mailbox for turn_done / next deliver to retry (not consuming)"
        )),
    }
}

/// POST `/api/message/consume_wakes?to=<id>` on swarmx-server → claimed set.
async fn consume_wakes(
    c: &reqwest::Client,
    swarmx_url: &str,
    agent_id: &str,
) -> Result<ConsumeWakesResponse> {
    let url = format!(
        "{}/api/message/consume_wakes",
        swarmx_url.trim_end_matches('/')
    );
    let resp = c
        .post(&url)
        .query(&[("to", agent_id)])
        .send()
        .await
        .context("consume_wakes send")?;
    if !resp.status().is_success() {
        return Err(anyhow!("consume_wakes HTTP {}", resp.status()));
    }
    resp.json().await.context("consume_wakes json")
}

/// Config handed to the per-agent driver task.
pub struct DriverCfg {
    pub serve_port: u16,
    pub agent_id: String,
    /// swarmx-server base URL (for consume_wakes + activity ingress).
    pub swarmx_url: String,
    /// First-turn prompt (system + task), already readiness-gated by the caller.
    pub bootstrap_prompt: String,
    /// Liveness handle: the driver exits once the agent leaves the registry.
    pub registry: Registry,
}

/// Spawn [`run_driver`] as a detached background task (the bootstrap path's
/// entry point).
pub fn run_driver_spawn(cfg: DriverCfg) {
    tokio::spawn(run_driver(cfg));
}

/// Long-lived driver for one reasonix agent. Spawned (detached) by the bootstrap
/// path once the readiness gate passes. Returns when the agent is gone or serve
/// is permanently unreachable.
pub async fn run_driver(cfg: DriverCfg) {
    let DriverCfg {
        serve_port,
        agent_id,
        swarmx_url,
        bootstrap_prompt,
        registry,
    } = cfg;

    if !wait_serve_ready(serve_port, Duration::from_secs(60)).await {
        tracing::warn!(agent = %agent_id, port = serve_port, "reasonix serve never came up; driver giving up");
        return;
    }

    if let Ok(c) = control_client() {
        if let Err(err) = set_yolo(&c, serve_port).await {
            tracing::warn!(agent = %agent_id, ?err, "reasonix: set yolo failed; tools may park on approval");
        }
    }

    if let Err(err) = deliver_turn(serve_port, &bootstrap_prompt).await {
        tracing::warn!(agent = %agent_id, ?err, "reasonix: bootstrap /submit failed");
    } else {
        tracing::info!(agent = %agent_id, port = serve_port, "reasonix: bootstrap submitted; following /events");
    }

    // Follow /events; reconnect while the agent is still registered.
    loop {
        if registry.get(&agent_id).is_none() {
            tracing::debug!(agent = %agent_id, "reasonix driver: agent gone; exiting");
            return;
        }
        match stream_events(serve_port, &agent_id, &swarmx_url, &registry).await {
            Ok(()) => {
                // Stream ended cleanly (serve closed it). If the agent is still
                // alive, reconnect after a short pause.
                if registry.get(&agent_id).is_none() {
                    return;
                }
                tracing::debug!(agent = %agent_id, "reasonix /events ended; reconnecting");
            }
            Err(err) => {
                if registry.get(&agent_id).is_none() {
                    return;
                }
                tracing::debug!(agent = %agent_id, ?err, "reasonix /events errored; reconnecting");
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Per-call activity bookkeeping (pairs a `running` row with its later `ok`).
struct ToolCall {
    seq: u32,
    label: String,
    started: Instant,
}

/// Open one `/events` SSE connection and process events until it ends/errors.
async fn stream_events(
    serve_port: u16,
    agent_id: &str,
    swarmx_url: &str,
    registry: &Registry,
) -> Result<()> {
    let c = stream_client()?;
    let mut resp = c
        .get(format!("{}/events", base(serve_port)))
        .send()
        .await
        .context("reasonix /events connect")?;
    if !resp.status().is_success() {
        return Err(anyhow!("reasonix /events HTTP {}", resp.status()));
    }

    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut seq: u32 = 0;
    let mut calls: HashMap<String, ToolCall> = HashMap::new();

    while let Some(chunk) = resp.chunk().await.context("reasonix /events read")? {
        buf.extend_from_slice(&chunk);
        // Split into complete lines; SSE frames are `data: {json}\n`.
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=nl).collect::<Vec<u8>>();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
            if !payload.starts_with('{') {
                continue;
            }
            let event: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            handle_event(
                &event, serve_port, agent_id, swarmx_url, &mut seq, &mut calls,
            )
            .await;
        }
        // Bound the buffer if a single frame is pathologically large.
        if buf.len() > 1_048_576 {
            buf.clear();
        }
        if registry.get(agent_id).is_none() {
            return Ok(());
        }
    }
    Ok(())
}

async fn handle_event(
    event: &Value,
    serve_port: u16,
    agent_id: &str,
    swarmx_url: &str,
    seq: &mut u32,
    calls: &mut HashMap<String, ToolCall>,
) {
    let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    match kind {
        "turn_done" => {
            // Turn-end: deliver any wakes that landed during the turn.
            match consume_and_submit(serve_port, swarmx_url, agent_id, None).await {
                Ok(true) => {
                    tracing::info!(agent = %agent_id, "reasonix: woke agent on turn_done (pending wakes)")
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::debug!(agent = %agent_id, ?err, "reasonix: turn_done consume/submit failed")
                }
            }
        }
        "tool_dispatch" => {
            let tool = match event.get("tool") {
                Some(t) => t,
                None => return,
            };
            let id = tool
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() || calls.contains_key(&id) {
                // Ignore the duplicate "partial" dispatch for the same call.
                return;
            }
            let label = tool_label(tool);
            let s = *seq;
            *seq += 1;
            calls.insert(
                id,
                ToolCall {
                    seq: s,
                    label: label.clone(),
                    started: Instant::now(),
                },
            );
            post_activity(swarmx_url, agent_id, "running", &label, s, None).await;
        }
        "tool_result" => {
            let tool = match event.get("tool") {
                Some(t) => t,
                None => return,
            };
            let id = tool.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(call) = calls.remove(id) {
                let dur = call.started.elapsed().as_millis() as u32;
                let phase = if tool.get("error").is_some() {
                    "error"
                } else {
                    "ok"
                };
                post_activity(
                    swarmx_url,
                    agent_id,
                    phase,
                    &call.label,
                    call.seq,
                    Some(dur),
                )
                .await;
            }
        }
        "usage" => {
            // reasonix reports per-turn token/cache usage natively. Accounting
            // ingestion is a follow-up; log it for now so it's observable.
            if let Some(u) = event.get("usage") {
                tracing::debug!(agent = %agent_id, usage = %u, "reasonix usage");
            }
        }
        "notice" => {
            if let Some(msg) = event.get("message").and_then(|v| v.as_str()) {
                tracing::debug!(agent = %agent_id, notice = %msg, "reasonix notice");
            }
        }
        _ => {}
    }
}

/// Best-effort POST of one activity row. Never throws into the driver.
async fn post_activity(
    swarmx_url: &str,
    agent_id: &str,
    phase: &str,
    label: &str,
    seq: u32,
    duration_ms: Option<u32>,
) {
    let Ok(c) = control_client() else { return };
    let url = format!(
        "{}/api/agent/{}/activity",
        swarmx_url.trim_end_matches('/'),
        urlencode(agent_id)
    );
    let mut body = json!({ "phase": phase, "label": label, "seq": seq });
    if let Some(d) = duration_ms {
        body["duration_ms"] = json!(d);
    }
    let _ = c.post(&url).json(&body).send().await;
}

/// Build a one-line label from a reasonix tool object: bare tool name (MCP
/// `mcp__<server>__` prefix stripped) plus the first salient arg value.
fn tool_label(tool: &Value) -> String {
    let raw = tool.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
    // Strip an `mcp__<server>__` prefix so `mcp__swarmx-swarm__swarm_write_blackboard`
    // shows as `swarm_write_blackboard`.
    let name = match raw.strip_prefix("mcp__") {
        Some(rest) => rest.splitn(2, "__").nth(1).unwrap_or(rest),
        None => raw,
    };
    let mut label = name.to_string();
    // `args` is a JSON STRING on the reasonix tool object; parse best-effort.
    if let Some(args_str) = tool.get("args").and_then(|v| v.as_str()) {
        if let Ok(args) = serde_json::from_str::<Value>(args_str) {
            const SALIENT: &[&str] = &[
                "key",
                "path",
                "file_path",
                "command",
                "cmd",
                "pattern",
                "query",
                "url",
                "to",
            ];
            for k in SALIENT {
                if let Some(v) = args.get(*k).and_then(|v| v.as_str()) {
                    if !v.is_empty() {
                        label.push(' ');
                        label.push_str(v);
                        break;
                    }
                }
            }
        }
    }
    if label.len() > 80 {
        label.truncate(79);
        label.push('…');
    }
    label
}

/// Minimal percent-encoding for an agent id in a URL path segment (ids are
/// `<cli>-<hex>`, but be safe).
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_label_strips_mcp_prefix_and_adds_salient_arg() {
        let tool = json!({
            "name": "mcp__swarmx-swarm__swarm_write_blackboard",
            "args": "{\"path\":\"design/api\",\"content\":\"...\"}"
        });
        assert_eq!(tool_label(&tool), "swarm_write_blackboard design/api");
    }

    #[test]
    fn tool_label_plain_tool_with_command() {
        let tool = json!({ "name": "bash", "args": "{\"command\":\"go test ./...\"}" });
        assert_eq!(tool_label(&tool), "bash go test ./...");
    }

    #[test]
    fn tool_label_no_args() {
        let tool = json!({ "name": "glob" });
        assert_eq!(tool_label(&tool), "glob");
    }

    #[test]
    fn wake_reason_mentions_blackboard_recipe() {
        let r = ConsumeWakesResponse {
            count: 2,
            ids: vec![1, 2],
            ..Default::default()
        }
        .continuation();
        assert!(r.contains("2 new wake"));
        assert!(r.contains("swarm_list_blackboard"));
    }
}
