//! Drive a Comate Zulu agent over its `zulu serve` HTTP+SSE control API.
//!
//! Like reasonix, zulu has no TUI to type into; its `serve` mode exposes a
//! small HTTP API + per-turn SSE streams (reverse-engineered from the v1.6.1
//! bundle + live-verified — see the P1.1 note in memory):
//!   - `GET  /health`                      → {status:"ok"} (serve bound)
//!   - `POST /session {license,query,model,mode,cwd,display,conversationId?}`
//!         → **SSE stream for THIS turn** (`data: {json}\n\n`), ends when the
//!         turn completes.
//!   - `POST /list-model` / `POST /inspect`
//! SSE event `type`s: `conversation-messages` (snapshots), `conversation-status`
//! (`conversationInfo.status` Running→**Completed** = TURN-END), `partial-
//! message-data`, `partial-message-elements` (`messageData.elements[].children[]
//! .type` in {REASON,TEXT,TOOL}; TEXT = answer, TOOL = tool activity).
//!
//! KEY DIFFERENCE from reasonix: reasonix keeps ONE persistent `/events` stream
//! across all turns and posts turns to `/submit`; zulu returns a FRESH stream
//! per `POST /session` turn. So this driver OWNS the conversation: it is the
//! sole turn-runner. Per-agent state lives in [`ZuluConv`] (conversation_id +
//! a `busy` flag). A turn runs to its SSE `Completed`, then pending wakes are
//! consumed (atomic) and, if any, the wake reason is run as the next turn. A
//! wake that lands while the agent is IDLE is delivered by the WakeCoordinator
//! via [`wake_if_idle`]; the atomic `consume_wakes` guarantees at most one path
//! ever submits. Continuation text is `ConsumeWakesResponse::continuation`.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use swarmx_protocol::rest::ConsumeWakesResponse;

use crate::registry::Registry;

/// Per-agent zulu conversation handle (stored on the `AgentSlot`). The driver
/// and the WakeCoordinator's [`wake_if_idle`] path share it: `busy` serialises
/// turns (zulu also serialises same-conversation runs itself, so this is the
/// belt to that suspenders) and `conversation_id` threads multi-turn context.
pub struct ZuluConv {
    /// The `zulu serve` HTTP port (also mirrored into `AgentSlot.serve_http_port`).
    pub serve_port: u16,
    /// Resolved model id/display-name for this agent (per-request, NOT a spawn
    /// arg — see cli-plugins/zulu.toml).
    pub model: String,
    /// Comate SaaS license (P1.4 wires the real source; empty = let zulu error,
    /// surfaced by the first-response watchdog).
    pub license: String,
    /// The agent's working directory (passed as `cwd` in each POST body).
    pub cwd: String,
    /// swarmx-server base URL (for consume_wakes + activity ingress). Stored so
    /// both the bootstrap driver and the WakeCoordinator's wake path can drive
    /// without threading it through every call.
    pub swarmx_url: String,
    /// Learned from the first turn's SSE, threaded into later turns.
    pub conversation_id: Mutex<Option<String>>,
    /// True while a turn is running (or its post-turn wake drain is in flight).
    pub busy: AtomicBool,
}

impl ZuluConv {
    pub fn new(
        serve_port: u16,
        model: String,
        license: String,
        cwd: String,
        swarmx_url: String,
    ) -> Self {
        Self {
            serve_port,
            model,
            license,
            cwd,
            swarmx_url,
            conversation_id: Mutex::new(None),
            busy: AtomicBool::new(false),
        }
    }
    fn conv_id(&self) -> Option<String> {
        self.conversation_id.lock().unwrap().clone()
    }
    fn set_conv_id(&self, id: &str) {
        let mut g = self.conversation_id.lock().unwrap();
        if g.is_none() {
            *g = Some(id.to_string());
        }
    }
}

fn base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Short control-call client (health / consume / activity).
fn control_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build zulu control http client")
}

/// Streaming client for a per-turn `/session` SSE response: NO total timeout
/// (a turn can run for minutes).
fn stream_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .build()
        .context("build zulu sse http client")
}

/// `GET /health` → serve answering? Single-shot reachability probe: true only
/// on a success response from the serve port (false = wedged / never bound).
async fn serve_reachable(port: u16) -> bool {
    let Ok(c) = control_client() else {
        return false;
    };
    matches!(
        c.get(format!("{}/health", base(port))).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// `GET /health` → serve bound? Polls until it answers or the window elapses.
async fn wait_serve_ready(port: u16, overall: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < overall {
        if serve_reachable(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// Build the `POST /session` body for one turn.
fn session_body(conv: &ZuluConv, prompt: &str, mode: &str) -> Value {
    let mut body = json!({
        "license": conv.license,
        "query": prompt,
        "model": conv.model,
        "mode": mode,
        "cwd": conv.cwd,
        "display": "event-stream",
    });
    if let Some(id) = conv.conv_id() {
        body["conversationId"] = json!(id);
    }
    body
}

/// Outcome of a one-turn usability probe (see [`verify_one_turn`]).
pub enum TurnProbe {
    /// The model produced output → the engine really ran a turn.
    Ok,
    /// The turn ended / timed out with no model output (wedged, no license, …).
    NoOutput,
    /// An auth/license error surfaced in the stream → needs a valid license.
    Auth(String),
}

/// Verified one-turn usability check for the engine probe. Drives the SAME
/// HTTP+SSE surface the live driver uses: `POST /session` a trivial `Ask` turn
/// and watch the stream for a `TEXT` element (real model output) before the
/// `conversationInfo.status` reaches `Completed`. Launch-only can't tell a valid
/// license from an invalid/expired one (serve binds either way); this can.
pub async fn verify_one_turn(
    serve_port: u16,
    model: &str,
    license: &str,
    cwd: &str,
    prompt: &str,
    total: Duration,
) -> TurnProbe {
    if !wait_serve_ready(serve_port, Duration::from_secs(10)).await {
        return TurnProbe::NoOutput;
    }
    let conv = ZuluConv::new(
        serve_port,
        model.to_string(),
        license.to_string(),
        cwd.to_string(),
        String::new(),
    );
    let Ok(sc) = stream_client() else {
        return TurnProbe::NoOutput;
    };
    let mut resp = match sc
        .post(format!("{}/session", base(serve_port)))
        .json(&session_body(&conv, prompt, "Ask"))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return TurnProbe::NoOutput,
    };

    let start = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut produced = false;
    let mut auth: Option<String> = None;
    loop {
        let Some(remaining) = total.checked_sub(start.elapsed()) else {
            break;
        };
        let chunk = match tokio::time::timeout(remaining, resp.chunk()).await {
            Ok(Ok(Some(c))) => c,
            _ => break,
        };
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=nl).collect::<Vec<u8>>();
            let payload = sse_payload(&line);
            let Some(payload) = payload else { continue };
            if auth.is_none() {
                let low = payload.to_lowercase();
                if low.contains("unauthorized")
                    || low.contains("license")
                    || low.contains("invalid")
                    || low.contains("quota")
                {
                    // Only latch if it also looks like an error frame.
                    if low.contains("error") || low.contains("fail") || low.contains("invalid") {
                        auth = Some(payload.chars().take(200).collect());
                    }
                }
            }
            let Ok(event) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            if event_has_text(&event) {
                produced = true;
            }
            if status_completed(&event) {
                return decide_probe(produced, auth);
            }
        }
        if buf.len() > 1_048_576 {
            buf.clear();
        }
    }
    decide_probe(produced, auth)
}

fn decide_probe(produced: bool, auth: Option<String>) -> TurnProbe {
    if let Some(d) = auth {
        TurnProbe::Auth(d)
    } else if produced {
        TurnProbe::Ok
    } else {
        TurnProbe::NoOutput
    }
}

/// Config handed to the per-agent driver task.
pub struct DriverCfg {
    pub conv: std::sync::Arc<ZuluConv>,
    pub agent_id: String,
    /// First-turn prompt (system + task), already readiness-gated by the caller.
    pub bootstrap_prompt: String,
    /// Liveness handle: the driver exits once the agent leaves the registry.
    pub registry: Registry,
}

/// Spawn the bootstrap driver as a detached background task.
pub fn run_driver_spawn(cfg: DriverCfg) {
    tokio::spawn(run_driver(cfg));
}

async fn run_driver(cfg: DriverCfg) {
    let DriverCfg {
        conv,
        agent_id,
        bootstrap_prompt,
        registry,
    } = cfg;

    if !wait_serve_ready(conv.serve_port, Duration::from_secs(60)).await {
        tracing::warn!(agent = %agent_id, port = conv.serve_port, "zulu serve never came up; driver giving up");
        return;
    }
    // Bootstrap turn: hold busy for its whole drain loop.
    conv.busy.store(true, Ordering::SeqCst);
    drive(&conv, &agent_id, &registry, bootstrap_prompt).await;
}

/// Run turns for this agent until it goes idle with no pending wakes. Enters
/// holding `conv.busy`; releases it before returning. Each turn runs to its SSE
/// `Completed`, then pending wakes are consumed (atomic) and, if any, the wake
/// reason is run as the next turn without releasing busy.
async fn drive(conv: &ZuluConv, agent_id: &str, registry: &Registry, first_prompt: String) {
    let mut prompt = first_prompt;
    let mode = "Agent";
    loop {
        if registry.get(agent_id).is_none() {
            conv.busy.store(false, Ordering::SeqCst);
            return;
        }
        if let Err(err) = run_turn(conv, agent_id, &prompt, mode).await {
            tracing::debug!(agent = %agent_id, ?err, "zulu turn errored");
        }
        // Turn done. While still holding busy, drain any wakes that landed.
        match consume_wakes(&conv.swarmx_url, agent_id).await {
            Ok(resp) if resp.count > 0 => {
                tracing::info!(agent = %agent_id, wakes = resp.count, "zulu: woke agent after turn (pending wakes)");
                prompt = resp.continuation();
                continue;
            }
            Ok(_) => {}
            Err(err) => tracing::debug!(agent = %agent_id, ?err, "zulu: post-turn consume failed"),
        }
        // No pending wakes → go idle. Further wakes arrive via wake_if_idle.
        conv.busy.store(false, Ordering::SeqCst);
        return;
    }
}

/// Run ONE turn: POST /session, consume its SSE to `Completed` (capturing the
/// conversationId, forwarding tool activity). Returns when the turn ends.
async fn run_turn(conv: &ZuluConv, agent_id: &str, prompt: &str, mode: &str) -> Result<()> {
    let sc = stream_client()?;
    let mut resp = sc
        .post(format!("{}/session", base(conv.serve_port)))
        .json(&session_body(conv, prompt, mode))
        .send()
        .await
        .context("zulu /session send")?;
    if !resp.status().is_success() {
        return Err(anyhow!("zulu /session HTTP {}", resp.status()));
    }

    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut seq: u32 = 0;
    let mut calls: HashMap<String, ToolCall> = HashMap::new();
    while let Some(chunk) = resp.chunk().await.context("zulu /session read")? {
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=nl).collect::<Vec<u8>>();
            let Some(payload) = sse_payload(&line) else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            if let Some(id) = conv_id_of(&event) {
                conv.set_conv_id(&id);
            }
            forward_tool_activity(&event, &conv.swarmx_url, agent_id, &mut seq, &mut calls).await;
            if status_completed(&event) {
                return Ok(()); // dropping `resp` closes the per-turn stream
            }
        }
        if buf.len() > 1_048_576 {
            buf.clear();
        }
    }
    Ok(()) // stream ended = turn done
}

/// Called by the WakeCoordinator for a zulu agent. If a turn is already running,
/// the driver's post-turn drain picks the wake up (return false). Otherwise
/// atomically claim busy, consume pending wakes, and run them as a turn. The
/// consume is gated on a serve reachability probe: consuming against a wedged /
/// never-bound serve would mark the wake READ yet never deliver it (the exact
/// net-loss reasonix hit live — see `reasonix_serve::wake_if_idle`).
pub async fn wake_if_idle(
    conv: std::sync::Arc<ZuluConv>,
    agent_id: &str,
    registry: &Registry,
) -> Result<bool> {
    // Acquire busy; if it was already held, a turn is in flight → its drain
    // handles the wake (atomic consume ⇒ no double).
    if conv.busy.swap(true, Ordering::SeqCst) {
        return Ok(false);
    }
    // Reachability pre-check BEFORE consuming: `consume_wakes` is atomic and
    // stamps the mailbox wake READ on the server, but the follow-up
    // `POST /session` against an unreachable serve then fails — NET-LOSING
    // the wake (marked read, never delivered; the waiter hangs forever).
    // Leave the wake un-consumed so the driver's post-turn drain (once serve
    // answers) or the next BlackboardChanged re-delivers it. The caller
    // (`deliver_wake`) logs this Err as a `warn!`, doesn't panic.
    if !serve_reachable(conv.serve_port).await {
        conv.busy.store(false, Ordering::SeqCst);
        return Err(anyhow!(
            "zulu serve unreachable on :{}; leaving wake in the mailbox for \
             the driver / next deliver to retry (not consuming)",
            conv.serve_port
        ));
    }
    match consume_wakes(&conv.swarmx_url, agent_id).await {
        Ok(resp) if resp.count > 0 => {
            let c = conv.clone();
            let aid = agent_id.to_string();
            let reg = registry.clone();
            let prompt = resp.continuation();
            // Drive holds busy for its whole life and releases at the end.
            tokio::spawn(async move { drive(&c, &aid, &reg, prompt).await });
            Ok(true)
        }
        Ok(_) => {
            conv.busy.store(false, Ordering::SeqCst);
            Ok(false)
        }
        Err(e) => {
            conv.busy.store(false, Ordering::SeqCst);
            Err(e)
        }
    }
}

// ─── SSE event helpers (zulu schema) ──────────────────────────────────────

/// Strip an SSE `data:` frame to its JSON payload, or None for non-JSON lines.
fn sse_payload(line: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    payload.starts_with('{').then(|| payload.to_string())
}

/// `conversationInfo.id` on any event, if present.
fn conv_id_of(event: &Value) -> Option<String> {
    event
        .get("conversationInfo")?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

/// True once the turn reaches a terminal `conversationInfo.status`.
fn status_completed(event: &Value) -> bool {
    matches!(
        event
            .get("conversationInfo")
            .and_then(|c| c.get("status"))
            .and_then(|s| s.as_str()),
        Some("Completed") | Some("Failed") | Some("Cancelled") | Some("Stopped")
    )
}

/// True if the event carries a streamed `TEXT` element (real model output).
fn event_has_text(event: &Value) -> bool {
    element_children(event).is_some_and(|ch| {
        ch.iter().any(|c| {
            c.get("type").and_then(|t| t.as_str()) == Some("TEXT")
                && c.get("content")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty())
        })
    })
}

/// The `messageData.elements[].children[]` array for a `partial-message-elements`
/// event, flattened across elements.
fn element_children(event: &Value) -> Option<Vec<Value>> {
    let elements = event.get("messageData")?.get("elements")?.as_array()?;
    let mut out = Vec::new();
    for el in elements {
        if let Some(children) = el.get("children").and_then(|c| c.as_array()) {
            out.extend(children.iter().cloned());
        }
    }
    Some(out)
}

/// Per-call activity bookkeeping (pairs a `running` row with its later result).
struct ToolCall {
    seq: u32,
    label: String,
    started: Instant,
}

/// Mirror zulu `TOOL` elements into the Activity view. A TOOL child that's
/// `end:false` starts a `running` row; the matching `end:true` closes it.
async fn forward_tool_activity(
    event: &Value,
    swarmx_url: &str,
    agent_id: &str,
    seq: &mut u32,
    calls: &mut HashMap<String, ToolCall>,
) {
    let Some(children) = element_children(event) else {
        return;
    };
    for child in children {
        if child.get("type").and_then(|t| t.as_str()) != Some("TOOL") {
            continue;
        }
        let id = child
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let ended = child.get("end").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ended {
            if calls.contains_key(&id) {
                continue; // already tracking this running call
            }
            let label = tool_label(&child);
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
        } else if let Some(call) = calls.remove(&id) {
            let dur = call.started.elapsed().as_millis() as u32;
            let phase = if child.get("error").is_some() {
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
}

/// One-line label from a zulu TOOL child: bare tool name (mcp prefix stripped)
/// plus the first salient arg. Best-effort — zulu's exact TOOL arg shape is
/// refined against live tool turns.
fn tool_label(tool: &Value) -> String {
    let raw = tool
        .get("name")
        .or_else(|| tool.get("toolName"))
        .and_then(|v| v.as_str())
        .unwrap_or("tool");
    let name = match raw.strip_prefix("mcp__") {
        Some(rest) => rest.splitn(2, "__").nth(1).unwrap_or(rest),
        None => raw,
    };
    let mut label = name.to_string();
    let args = tool
        .get("args")
        .or_else(|| tool.get("input"))
        .or_else(|| tool.get("params"));
    let args = match args {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
        Some(v) => Some(v.clone()),
        None => None,
    };
    if let Some(args) = args {
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
    if label.chars().count() > 80 {
        label = label.chars().take(79).collect::<String>() + "…";
    }
    label
}

// ─── swarmx-server control calls (shared with the reasonix contract) ───────

/// POST `/api/message/consume_wakes?to=<id>` → claimed set (atomic).
async fn consume_wakes(swarmx_url: &str, agent_id: &str) -> Result<ConsumeWakesResponse> {
    let c = control_client()?;
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

/// Minimal percent-encoding for an agent id in a URL path segment.
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
    fn status_completed_detects_terminal_states() {
        let running = json!({"conversationInfo": {"status": "Running"}});
        let done = json!({"conversationInfo": {"status": "Completed"}});
        assert!(!status_completed(&running));
        assert!(status_completed(&done));
        assert!(status_completed(
            &json!({"conversationInfo":{"status":"Failed"}})
        ));
    }

    #[test]
    fn event_has_text_only_for_nonempty_text_child() {
        let text = json!({"messageData":{"elements":[{"children":[
            {"type":"TEXT","content":"hi there"}]}]}});
        let reason = json!({"messageData":{"elements":[{"children":[
            {"type":"REASON","content":"thinking..."}]}]}});
        let empty = json!({"messageData":{"elements":[{"children":[
            {"type":"TEXT","content":""}]}]}});
        assert!(event_has_text(&text));
        assert!(!event_has_text(&reason));
        assert!(!event_has_text(&empty));
    }

    #[test]
    fn conv_id_extracted() {
        let ev = json!({"conversationInfo": {"id": "abc-123", "status": "Running"}});
        assert_eq!(conv_id_of(&ev).as_deref(), Some("abc-123"));
        assert_eq!(conv_id_of(&json!({})), None);
    }

    #[test]
    fn tool_label_strips_mcp_prefix_and_adds_salient_arg() {
        let tool = json!({
            "name": "mcp__swarmx-swarm__swarm_write_blackboard",
            "args": {"path": "design/api", "content": "..."}
        });
        assert_eq!(tool_label(&tool), "swarm_write_blackboard design/api");
    }

    #[test]
    fn tool_label_args_as_json_string() {
        let tool = json!({ "name": "bash", "args": "{\"command\":\"go test ./...\"}" });
        assert_eq!(tool_label(&tool), "bash go test ./...");
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

    #[test]
    fn session_body_includes_conv_id_after_set() {
        let conv = ZuluConv::new(
            8790,
            "M".into(),
            "L".into(),
            "/tmp".into(),
            "http://x".into(),
        );
        let b0 = session_body(&conv, "hi", "Agent");
        assert!(b0.get("conversationId").is_none());
        assert_eq!(b0["display"], "event-stream");
        conv.set_conv_id("cid-1");
        let b1 = session_body(&conv, "hi", "Agent");
        assert_eq!(b1["conversationId"], "cid-1");
    }

    #[tokio::test]
    async fn wake_if_idle_does_not_consume_when_serve_unreachable() {
        // Mock swarmx-server consume endpoint that COUNTS calls: if the wake
        // were (wrongly) consumed, this counter would move.
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let app = axum::Router::new().route(
            "/api/message/consume_wakes",
            axum::routing::post(move || {
                let hits = hits_in_handler.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({ "count": 1 }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let swarmx_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // A port nothing listens on stands in for a wedged / never-bound serve.
        let dead_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let conv = std::sync::Arc::new(ZuluConv::new(
            dead_port,
            "M".into(),
            "L".into(),
            "/tmp".into(),
            swarmx_url,
        ));
        let registry = Registry::new();
        let err = wake_if_idle(conv.clone(), "zulu-test", &registry)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("unreachable"),
            "refused wake must surface the reachability error: {err:#}"
        );
        assert!(
            !conv.busy.load(Ordering::SeqCst),
            "busy released after a refused wake"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "wake must NOT be consumed while serve is unreachable"
        );
    }
}
