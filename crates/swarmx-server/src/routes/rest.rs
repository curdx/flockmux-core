//! REST endpoints. Loopback-only; no auth (per user decision — local single-user).

use crate::plugins::{CliPlugin, PluginRegistry};
use crate::registry::LifecycleEvent;
use crate::spawn::{spawn_agent, WorkspaceLayout};
use crate::spells;
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use swarmx_protocol::rest::{
    AgentActivityRecord, AgentInfo, CliInstallHint, CliPluginInfo, RunSpellAgent, RunSpellRequest,
    RunSpellResponse, SpawnAgentRequest, SpawnAgentResponse, SpawnWorkerRequest,
    SpawnWorkerResponse, SpellAgentInfo, SpellInfo,
};
use swarmx_protocol::ws_swarm::{AgentState, SwarmEvent};
use swarmx_recorder::{Recorder, RecorderConfig};
use swarmx_storage::{NewAgent, NewRecording, NewSpellRun, NewWorker, ThreadRecord};
use uuid::Uuid;

/// First-response watchdog window: how long after `ShimReady` an agent may
/// produce zero observable sign of life (no message, no tool activity, no
/// token usage) before we declare it wedged and flip it to `AgentState::Error`.
/// The design's honesty bar — the orchestrator's `init` greet normally lands
/// well inside this, so the window is generous enough to avoid false alarms on
/// a slow first turn while still surfacing a never-started agent fast.
///
/// Per-engine, NOT flat — because the window is only meaningful for engines that
/// can be legitimately silent during a slow first turn:
///   - claude/codex stream JSONL transcript activity within seconds (the
///     `transcript` tailer touches activity per tool step), so they're never
///     silent for long — a tight 90s window catches a real wedge fast.
///   - opencode (TUI cold-start + first model call) and reasonix (serve + first
///     SSE turn) have NO transcript tailer and can sit genuinely quiet for
///     60-90s while working. The codebase's own budget for that first turn is
///     ~100s (see `engine_probe::probe_timeout` opencode=100s / OPENCODE_TURN_
///     TIMEOUT=75s), so 150s ≈ 1.5× that — comfortably above a slow-but-fine
///     first turn, still bounded for a true wedge.
///   - kimi likewise has no transcript tailer (PTY engine, no session-id flag
///     to pin one to): its first-turn evidence is mailbox/blackboard writes,
///     which can trail a long think. Same 150s budget as the other
///     transcript-less engines.
/// Keep the opencode value in sync with `opencode_tui::deliver_bootstrap`'s
/// overall window (they're the coupled "did opencode start its first turn" pair).
fn first_response_watchdog_ms(engine: &str) -> u64 {
    match engine {
        "opencode" | "reasonix" | "kimi" => 150_000,
        _ => 90_000,
    }
}

/// See `crate::agent_lifecycle::gate_should_shield_watchdog`.

/// NeedsYou「该醒没醒」stalled 信号的静默窗口:一封未读邮件要等待这么久、
/// 且 agent 同期毫无活动,才允许怀疑唤醒链断了。10 分钟是刻意保守的选择 —
/// 0.3.0 的 suspected-stuck 因为误报(狼来了:codex 明明还在出 exec 事件被标
/// 「疑似卡住」)被整体撤出收件箱,这一版零误报优先:
///   - 空闲等活的 worker 没有未读邮件,永不命中;
///   - 正在干活的 worker 活动时间戳是新的,永不命中;
///   - 只有「有东西等着它读 + 它长时间没动静」才命中 —— 即使命中,UI 也只说
///     「可能卡住」。10min 留足了一个正常回合读信的余量(wake-check 在回合末
///     触发,长回合里到的邮件回合末才读),又把真断链的暴露延迟控制在可接受范围。
const STALLED_UNREAD_SILENCE_MS: i64 = 10 * 60 * 1000;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Closest candidate to `target` within edit distance 3 — drives the
/// "did you mean 'frontend'?" hint when an unknown role slug is spawned.
fn closest_match(target: &str, candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .map(|c| (levenshtein(target, c), c))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c.clone())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// In-flight handoff claims so two concurrent `spawn_worker` calls cannot both
/// mint the class key for the same role before either persists a worker row.
struct HandoffClaim {
    workspace_id: String,
    thread_slug: String,
    role_slug: String,
    handoff_signal: String,
}

fn handoff_claims() -> &'static std::sync::Mutex<Vec<HandoffClaim>> {
    static M: std::sync::OnceLock<std::sync::Mutex<Vec<HandoffClaim>>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

struct HandoffReservationGuard {
    key: String,
}

impl Drop for HandoffReservationGuard {
    fn drop(&mut self) {
        if let Ok(mut claims) = handoff_claims().lock() {
            claims.retain(|c| c.handoff_signal != self.key);
        }
    }
}

fn lock_claims() -> std::sync::MutexGuard<'static, Vec<HandoffClaim>> {
    handoff_claims().lock().unwrap_or_else(|e| e.into_inner())
}

/// Unique graph node for cycle detection. Must NOT be `role_slug` — two parallel
/// researchers share a role and would clobber each other in a HashMap.
fn handoff_graph_node(role_slug: &str, handoff_signal: &str) -> String {
    if handoff_signal.is_empty() {
        role_slug.to_string()
    } else {
        handoff_signal.to_string()
    }
}

/// Live (not killed / not exited) workers in this workspace direction.
async fn live_direction_workers(
    state: &AppState,
    workspace_id: &str,
    thread_id: Option<&str>,
) -> Vec<swarmx_storage::WorkerRecord> {
    let sibling_ids: Vec<String> = match state.store.list_agents().await {
        Ok(rows) => rows
            .into_iter()
            .filter(|a| {
                a.killed_at.is_none()
                    && a.workspace_id.as_deref() == Some(workspace_id)
                    && a.thread_id.as_deref() == thread_id
                    && a.role != "orchestrator"
            })
            .map(|a| a.id)
            .collect(),
        Err(e) => {
            tracing::warn!(
                ?e,
                "handoff: list_agents failed; treating sibling set as empty"
            );
            return Vec::new();
        }
    };
    if sibling_ids.is_empty() {
        return Vec::new();
    }
    match state.store.list_workers_by_ids(sibling_ids).await {
        Ok(map) => map.into_values().collect(),
        Err(e) => {
            tracing::warn!(?e, "handoff: list_workers_by_ids failed");
            Vec::new()
        }
    }
}

/// Live orchestrator in this direction, if any. Thread match is exact on the
/// normalized key (`None` ≡ `""` for legacy rows without a thread stamp).
async fn live_orchestrator_in_direction(
    state: &AppState,
    workspace_id: &str,
    thread_id: Option<&str>,
) -> Option<swarmx_storage::AgentRecord> {
    let want = thread_id.unwrap_or("");
    let agents = state.store.list_agents().await.ok()?;
    agents.into_iter().find(|a| {
        a.role == "orchestrator"
            && a.killed_at.is_none()
            && a.shim_exit_at.is_none()
            && a.workspace_id.as_deref() == Some(workspace_id)
            && a.thread_id.as_deref().unwrap_or("") == want
    })
}

/// Spawn-time dependency-graph validation + key minting (P0-A), pure so it can
/// be unit-tested without an HTTP server. Resolves each typed `consumes` ref to
/// the producer's minted blackboard key(s), after verifying the producer role
/// exists and declares the requested output kind. Bindings follow
/// [`crate::roles::consume_keys`]: currently live producers, or the class key
/// if none. Returns the minted `depends_on` keys, or a human error (mapped to
/// 400 by the caller).
fn resolve_consumes_to_deps(
    registry: &crate::roles::RoleRegistry,
    role_slug: &str,
    consumes: &[swarmx_protocol::rest::ConsumeRef],
    workspace_id: &str,
    thread_slug: &str,
    live: &[crate::roles::LiveProducer],
) -> Result<Vec<String>, String> {
    let mut depends_on = Vec::with_capacity(consumes.len());
    for c in consumes {
        let from = c.from_role.trim();
        let kind = c.kind.trim();
        if from.is_empty() {
            return Err("consumes entry has empty from_role".to_string());
        }
        if from == role_slug {
            return Err(format!(
                "role '{role_slug}' cannot consume its own output (self-dependency)"
            ));
        }
        let producer = registry.get(from).ok_or_else(|| {
            let valid = registry.ids();
            let mut msg = format!("consumes references unknown role '{from}'");
            if let Some(s) = closest_match(from, &valid) {
                msg.push_str(&format!(" — did you mean '{s}'?"));
            }
            msg.push_str(&format!(" valid roles: {valid:?}"));
            msg
        })?;
        let producer_kinds: Vec<String> = if producer.manifest.produces.is_empty() {
            vec!["done".to_string()]
        } else {
            producer.manifest.produces.clone()
        };
        if !producer_kinds.iter().any(|k| k == kind) {
            return Err(format!(
                "role '{from}' does not produce kind '{kind}' — it produces {producer_kinds:?}"
            ));
        }
        depends_on.extend(crate::roles::consume_keys(
            workspace_id,
            thread_slug,
            from,
            kind,
            live,
        ));
    }
    Ok(depends_on)
}

/// Append the server-minted handoff key(s) to the orchestrator-authored worker
/// prompt, so the worker writes the canonical key verbatim instead of inventing
/// one — the F3 drift class is designed away (P0-A).
///
/// `done_checks` is the W2-1 verify gate (opt-in): when non-empty the worker is
/// TOLD its delivery will be objectively re-run by the server, so it can run
/// the checks itself before declaring done. Empty (the default) adds nothing —
/// the prompt is byte-identical to before the gate existed.
fn build_worker_prompt(
    base: &str,
    success_keys: &[String],
    error_key: &str,
    dep_keys: &[String],
    done_checks: &[String],
    progress_key: Option<&str>,
) -> String {
    let mut out = base.to_string();

    // INPUTS / wait-gate: a worker is bootstrapped immediately on spawn, but its
    // typed dependencies may not be on the blackboard yet. Without this block it
    // would act prematurely (and, worse, write its handoff key → auto-killed
    // before the real work). Tell it to check its inputs first and STOP (without
    // writing handoff) if any are missing; the WakeCoordinator re-wakes it when
    // they land. A `<key>.error` means the upstream failed — handle, don't hang.
    if !dep_keys.is_empty() {
        let deps = dep_keys
            .iter()
            .map(|k| format!("  - {k}"))
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!(
            "\n\n──────────────────────────────────────────────────────────────\n\
             INPUTS — this task depends on other agents' output. BEFORE doing \
             anything, use swarm_list_blackboard / swarm_read_blackboard to check \
             for ALL of these keys:\n{deps}\n\
             If ANY key is missing: do NOT start the task and do NOT write your \
             handoff key. Reply in one line that you are waiting for inputs, then \
             STOP — swarmx re-wakes you automatically the moment they appear. \
             A `<key>.error` (instead of the key) means that upstream FAILED: \
             handle the failure path, do not wait forever. Only proceed with the \
             task once EVERY input key is present.\n"
        ));
    }

    // HANDOFF: the server-minted keys this worker writes. Copy verbatim.
    if !success_keys.is_empty() {
        let keys = success_keys
            .iter()
            .map(|k| format!("  - {k}"))
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!(
            "\n──────────────────────────────────────────────────────────────\n\
             HANDOFF (managed by swarmx — copy these keys VERBATIM via \
             swarm_write_blackboard; do NOT invent or alter them):\n\
             • On SUCCESS (only when the task is actually done), write your result to:\n{keys}\n\
             • On FAILURE/abort, write to `{error_key}` instead, so dependents \
             fail loudly rather than hang forever.\n"
        ));
    }

    if let Some(progress_key) = progress_key {
        out.push_str(&format!(
            "\n──────────────────────────────────────────────────────────────\n\
             PROGRESS BREADCRUMBS (managed by swarmx — copy this key VERBATIM):\n\
             After each meaningful milestone, overwrite `{progress_key}` with one line \
             `<HH:MM> <status>`. Parallel workers of the same role must not share a \
             progress key; this path is unique to you.\n"
        ));
    }

    // VERIFY GATE (W2-1, only when the role declared done_checks): telling the
    // worker its handoff is objectively graded changes behaviour for the better
    // — it runs the checks itself BEFORE declaring done instead of trusting its
    // own claim, and a bounce no longer surprises it.
    if !done_checks.is_empty() {
        let checks = done_checks
            .iter()
            .map(|c| format!("  - {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!(
            "\n──────────────────────────────────────────────────────────────\n\
             VERIFY GATE (managed by swarmx): when you write your handoff key, the \
             server runs these objective checks in your working directory and only \
             accepts the delivery if every one exits 0:\n{checks}\n\
             Run them yourself BEFORE writing the handoff key. If a check fails, the \
             delivery is bounced back to you with the failure output — fix it and \
             re-write the key; if it is genuinely unsatisfiable, write the error key \
             above and report the blocker.\n"
        ));
    }

    out
}

pub async fn list_plugins(State(state): State<AppState>) -> impl IntoResponse {
    let mut plugins = Vec::new();
    for p in state.plugins.list() {
        plugins.push(cli_plugin_info(p).await);
    }
    Json(plugins)
}

/// `POST /api/plugins/probe` — kick a background real-usability probe of every
/// engine: actually start each CLI over PTY and see whether it can run (not just
/// "is the binary installed"). Returns 202 immediately; verdicts land in
/// `~/.swarmx/engine-probe.json` as each completes (it's slow — opencode cold
/// start alone is up to ~90s — so it must not block the request).
pub async fn probe_engines(State(state): State<AppState>) -> impl IntoResponse {
    // Reading the flag here only shapes the response message; the real
    // single-sweep guarantee is `try_begin` inside `probe_all`, which no-ops a
    // duplicate even if two POSTs race past this check.
    let already = crate::engine_probe::is_probing();
    let plugins: Vec<crate::plugins::CliPlugin> =
        state.plugins.list().into_iter().cloned().collect();
    let count = plugins.len();
    let shim = state.shim_path.clone();
    let mcp = state.mcp_bin.clone();
    let url = state.server_url.clone();
    tokio::spawn(async move {
        crate::engine_probe::probe_all(&plugins, &shim, &mcp, &url).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": if already { "already_probing" } else { "probing" },
            "engines": count,
        })),
    )
}

/// `GET /api/plugins/probe` — the cached real-usability verdicts plus whether a
/// sweep is in flight right now. The frontend reads this on mount (stale cache
/// shown immediately) and polls it while `probing` is true to pick up verdicts
/// as each engine completes (stale-while-revalidate).
pub async fn probe_status() -> impl IntoResponse {
    Json(serde_json::json!({
        "probing": crate::engine_probe::is_probing(),
        "engines": crate::engine_probe::cached_results(),
    }))
}

async fn cli_plugin_info(p: &CliPlugin) -> CliPluginInfo {
    let resolved_path = crate::runtime_path::resolve_executable(&p.binary);
    let version = match resolved_path.as_deref() {
        Some(path) => probe_cli_version(path).await,
        None => None,
    };
    CliPluginInfo {
        id: p.id.clone(),
        display_name: p.display_name.clone(),
        binary: p.binary.clone(),
        installed: resolved_path.is_some(),
        resolved_path: resolved_path.map(|p| p.to_string_lossy().into_owned()),
        version,
        install: install_hint_for(p),
        input_settle_ms: p.input_settle_ms,
        default_model: p.default_model.clone(),
    }
}

async fn probe_cli_version(binary: &FsPath) -> Option<String> {
    use tokio::time::{timeout, Duration};

    let output = crate::runtime_path::tool_command_async(binary)
        .arg("--version")
        .kill_on_drop(true)
        .output();

    let output = timeout(Duration::from_secs(3), output).await.ok()?.ok()?;
    let raw = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let line = raw.lines().find(|l| !l.trim().is_empty())?.trim();
    Some(line.chars().take(200).collect())
}

fn install_hint_for(p: &CliPlugin) -> Option<CliInstallHint> {
    match p.id.as_str() {
        "codex" => Some(CliInstallHint {
            title: "Install Codex CLI".to_string(),
            summary: "Official OpenAI installer first; npm and Homebrew are useful fallbacks."
                .to_string(),
            docs_url: "https://github.com/openai/codex".to_string(),
            commands: vec![
                "curl -fsSL https://chatgpt.com/codex/install.sh | sh".to_string(),
                "npm install -g @openai/codex".to_string(),
                "brew install --cask codex".to_string(),
            ],
            verify_command: Some("codex --version".to_string()),
            login_command: Some("codex login".to_string()),
        }),
        "claude" => Some(CliInstallHint {
            title: "Install Claude Code".to_string(),
            summary: "Official native installer first; Homebrew is supported and npm is now a deprecated fallback."
                .to_string(),
            docs_url: "https://code.claude.com/docs/en/setup".to_string(),
            commands: vec![
                "curl -fsSL https://claude.ai/install.sh | bash".to_string(),
                "brew install --cask claude-code".to_string(),
                "npm install -g @anthropic-ai/claude-code".to_string(),
            ],
            verify_command: Some("claude --version".to_string()),
            login_command: Some("claude".to_string()),
        }),
        "opencode" => Some(CliInstallHint {
            title: "Install OpenCode".to_string(),
            summary: "Official install script first; npm and Homebrew are useful fallbacks."
                .to_string(),
            docs_url: "https://opencode.ai/docs/".to_string(),
            commands: vec![
                "curl -fsSL https://opencode.ai/install | bash".to_string(),
                "npm install -g opencode-ai".to_string(),
                "brew install opencode".to_string(),
            ],
            verify_command: Some("opencode --version".to_string()),
            login_command: Some("opencode auth login".to_string()),
        }),
        "reasonix" => Some(CliInstallHint {
            title: "Install Reasonix".to_string(),
            summary: "DeepSeek-native coding agent. Install via npm (the @next tag \
                      is the current 1.x build), then provide a DeepSeek API key — \
                      either `export DEEPSEEK_API_KEY=sk-...` before starting \
                      swarmx, or run `reasonix setup` to save it."
                .to_string(),
            docs_url: "https://reasonix.io/docs/".to_string(),
            commands: vec![
                "npm install -g reasonix@next".to_string(),
                "brew install esengine/reasonix/reasonix".to_string(),
            ],
            verify_command: Some("reasonix version".to_string()),
            login_command: Some("reasonix setup".to_string()),
        }),
        "zulu" => Some(CliInstallHint {
            title: "Install Comate Zulu".to_string(),
            summary: "多模型编码 CLI —— 一把 Comate license 跑十余个模型（多模对比 / \
                      竞赛的多模型来源）。npm 全局安装后，在下方填入 Comate license 即可。"
                .to_string(),
            docs_url: "https://www.npmjs.com/package/@comate/zulu".to_string(),
            commands: vec!["npm install -g @comate/zulu".to_string()],
            verify_command: Some("zulu --version".to_string()),
            login_command: None,
        }),
        "kimi" => Some(CliInstallHint {
            title: "Install Kimi Code".to_string(),
            summary: "Official install script first (no Node.js required); npm is the \
                      alternative (needs Node ≥ 22.19). Sign in via the OAuth device \
                      flow (`kimi login`) or a Kimi Platform API key."
                .to_string(),
            docs_url: "https://www.kimi.com/code/docs/en/kimi-code-cli/guides/getting-started.html"
                .to_string(),
            commands: vec![
                "curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash".to_string(),
                "npm install -g @moonshot-ai/kimi-code".to_string(),
            ],
            verify_command: Some("kimi --version".to_string()),
            login_command: Some("kimi login".to_string()),
        }),
        _ => None,
    }
}

/// Build an SSE event from a JSON value (empty event on the impossible serialize
/// failure, so a bad line never aborts the install stream).
fn sse_event(v: serde_json::Value) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .json_data(v)
        .unwrap_or_default()
}

/// `POST /api/plugins/:id/install` — run this engine's ONE-CLICK install command
/// and stream its output as SSE. The command is `install_hint_for(plugin)
/// .commands[0]` — a fixed, server-owned string; the ONLY client input is the
/// (validated) plugin id, never a command. Runs with the augmented PATH (so a
/// packaged GUI build finds node/npm) and `current_dir($HOME)` (so a CWD=`/`
/// build never writes a lockfile at `/`). On the child's exit it re-resolves the
/// binary + version and emits a terminal `done` event. Loopback + the Origin
/// guard are the trust boundary (same as `/ws/terminal`).
pub async fn install_plugin(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let plugin = state.plugins.get(&id).cloned();
    let resolved = plugin.as_ref().and_then(install_hint_for);
    let (cmd, binary) = match (resolved, plugin) {
        (Some(h), Some(p)) if !h.commands.is_empty() => (h.commands[0].clone(), p.binary.clone()),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("no one-click install for engine: {id}")})),
            )
                .into_response();
        }
    };

    let (tx, rx) = futures::channel::mpsc::unbounded::<axum::response::sse::Event>();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let _ = tx.unbounded_send(sse_event(
            json!({"type": "line", "text": format!("$ {cmd}")}),
        ));

        let home =
            crate::runtime_path::swarmx_home().unwrap_or_else(|| std::path::PathBuf::from("."));
        let spawned = crate::runtime_path::shell_command_async(&cmd)
            .current_dir(&home)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.unbounded_send(sse_event(
                    json!({"type": "done", "ok": false, "installed": false, "error": e.to_string()}),
                ));
                return;
            }
        };

        // Stream stdout + stderr line-by-line as they arrive.
        let mut readers = Vec::new();
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = tx.clone();
            readers.push(tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(pipe).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let _ = tx.unbounded_send(sse_event(json!({"type": "line", "text": l})));
                }
            }));
        }
        let status = child.wait().await;
        for r in readers {
            let _ = r.await;
        }

        let ok = status.map(|s| s.success()).unwrap_or(false);
        let resolved = crate::runtime_path::resolve_executable(&binary);
        let version = match &resolved {
            Some(p) => probe_cli_version(p).await,
            None => None,
        };
        let _ = tx.unbounded_send(sse_event(json!({
            "type": "done",
            "ok": ok,
            "installed": resolved.is_some(),
            "version": version,
        })));
    });

    use futures::StreamExt;
    axum::response::sse::Sse::new(rx.map(Ok::<_, std::convert::Infallible>))
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn missing_cli_install_message(plugin: &CliPlugin) -> String {
    let mut msg = format!(
        "{} CLI binary `{}` is not installed or not visible on swarmx's runtime PATH.",
        plugin.display_name, plugin.binary
    );
    if let Some(hint) = install_hint_for(plugin) {
        msg.push_str("\n\nRecommended install commands:\n");
        for command in &hint.commands {
            msg.push_str("- ");
            msg.push_str(command);
            msg.push('\n');
        }
        if let Some(login) = hint.login_command {
            msg.push_str("After install/login: ");
            msg.push_str(&login);
            msg.push('\n');
        }
        msg.push_str("Docs: ");
        msg.push_str(&hint.docs_url);
    }
    msg
}

fn missing_all_cli_install_message(requested: &CliPlugin, registry: &PluginRegistry) -> String {
    let mut msg = missing_cli_install_message(requested);
    let mut alternatives: Vec<String> = registry
        .list()
        .into_iter()
        .filter(|p| p.id != requested.id)
        .filter_map(|p| {
            install_hint_for(p).map(|hint| {
                let command = hint
                    .commands
                    .first()
                    .cloned()
                    .unwrap_or_else(|| format!("Install {}", p.display_name));
                format!("{}: {}", p.display_name, command)
            })
        })
        .collect();
    alternatives.sort();
    if !alternatives.is_empty() {
        msg.push_str("\n\nOther supported AI engines you can install:\n");
        for alt in alternatives {
            msg.push_str("- ");
            msg.push_str(&alt);
            msg.push('\n');
        }
    }
    msg
}

fn cli_plugin_installed(plugin: &CliPlugin) -> bool {
    crate::runtime_path::resolve_executable(&plugin.binary).is_some()
}

fn fallback_candidate_order(plugin: &CliPlugin) -> (u8, &str) {
    // Codex is the safest automatic fallback because it stays on the CLI-account
    // surface and runs over the same PTY path as the other engines.
    let preferred = if plugin.id == "codex" { 0 } else { 1 };
    (preferred, plugin.id.as_str())
}

fn select_spawn_plugin_with<'a>(
    registry: &'a PluginRegistry,
    requested_cli: &str,
    is_installed: &impl Fn(&CliPlugin) -> bool,
) -> Result<(&'a CliPlugin, Option<&'a CliPlugin>), (StatusCode, String)> {
    let requested = registry.get(requested_cli).ok_or_else(|| {
        // Mirror the unknown-ROLE error (closest_match + valid list) so an LLM
        // that typo'd the cli (`claud`) or named a nonexistent engine (`gpt4`)
        // gets the same self-correctable hint instead of a dead-end string.
        let valid: Vec<String> = registry.list().into_iter().map(|p| p.id.clone()).collect();
        let mut msg = format!("unknown cli plugin: {requested_cli}");
        if let Some(s) = closest_match(requested_cli, &valid) {
            msg.push_str(&format!(" — did you mean '{s}'?"));
        }
        msg.push_str(&format!(" valid engines: {valid:?}"));
        (StatusCode::NOT_FOUND, msg)
    })?;
    if is_installed(requested) {
        return Ok((requested, None));
    }

    let mut fallbacks: Vec<&CliPlugin> = registry
        .list()
        .into_iter()
        .filter(|p| p.id != requested.id)
        .filter(|p| !p.requires_explicit_billing_opt_in)
        .filter(|p| is_installed(p))
        .collect();
    fallbacks.sort_by_key(|p| fallback_candidate_order(p));
    if let Some(plugin) = fallbacks.into_iter().next() {
        return Ok((plugin, Some(requested)));
    }

    Err((
        StatusCode::BAD_REQUEST,
        missing_all_cli_install_message(requested, registry),
    ))
}

fn select_spawn_plugin<'a>(
    registry: &'a PluginRegistry,
    requested_cli: &str,
) -> Result<(&'a CliPlugin, Option<&'a CliPlugin>), (StatusCode, String)> {
    select_spawn_plugin_with(registry, requested_cli, &cli_plugin_installed)
}

/// `POST /api/agent/:id/mcp-ready` — called by the agent's own `swarmx-mcp`
/// subprocess once the CLI has fetched its tool list (per the MCP lifecycle,
/// that's the moment the `swarm_*` tools become visible to the model). Flips
/// the slot's `mcp_ready` gate so the deferred bootstrap can inject the prompt
/// immediately instead of waiting out a fixed timeout — the readiness-probe
/// pattern, replacing the old fixed 2500ms MCP-settle sleep. Idempotent;
/// 404 if the agent isn't (or is no longer) live.
pub async fn mcp_ready(State(state): State<AppState>, Path(agent_id): Path<String>) -> StatusCode {
    match state.registry.get(&agent_id) {
        Some(slot) => {
            slot.lock().mcp_ready.send_replace(true);
            state.swarm.publish_event(SwarmEvent::AgentStage {
                agent_id: agent_id.clone(),
                stage: "mcp_ready".into(),
                at: now_ms(),
            });
            tracing::debug!(agent = %agent_id, "mcp-ready signalled by swarmx-mcp");
            StatusCode::NO_CONTENT
        }
        None => StatusCode::NOT_FOUND,
    }
}

pub async fn spawn(
    State(state): State<AppState>,
    Json(req): Json<SpawnAgentRequest>,
) -> Result<Json<SpawnAgentResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Step 3: workspace_id is now mandatory. The frontend always passes
    // the active workspace's id; orphan `+ Claude` clicks must route
    // through CreateWizard first.
    let workspace_id = req.workspace_id.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "workspace_id required (create or pick a workspace first)"})),
        )
    })?;
    let ws = state
        .store
        .get_workspace_by_id(workspace_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("workspace {workspace_id} not found")})),
            )
        })?;
    // PerAgent root defaults to the workspace's cwd so the per-agent
    // subdir lands inside the user's chosen project tree. Callers can
    // still override with `req.workspace` (e.g. tests pinning to /tmp).
    let workspace_root = req
        .workspace
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&ws.cwd));
    let resolved_thread_id = match req.thread_id {
        Some(tid) => state
            .store
            .get_thread(tid)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            })?
            .filter(|t| t.deleted_at.is_none() && t.workspace_id == ws.id)
            .map(|t| t.id),
        None => state
            .store
            .list_threads(ws.id.clone())
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            })?
            .into_iter()
            .next()
            .map(|t| t.id),
    };
    // Captain singleton: never spawn a second live orchestrator in this
    // direction. Return the existing one — zero user burden (no "already
    // running" error to decode).
    if req.role.as_deref() == Some("orchestrator") {
        if let Some(existing) =
            live_orchestrator_in_direction(&state, &ws.id, resolved_thread_id.as_deref()).await
        {
            tracing::info!(
                agent = %existing.id,
                workspace = %ws.id,
                "POST /api/agent: reusing live orchestrator (singleton)"
            );
            let billing = state
                .plugins
                .get(&existing.cli)
                .map(|p| billing_surface_token(p.billing_surface))
                .unwrap_or_else(|| "unknown".into());
            return Ok(Json(SpawnAgentResponse {
                agent_id: existing.id,
                cli: existing.cli,
                role: existing.role,
                workspace: existing.workspace,
                fallback_from: None,
                billing_surface: Some(billing),
            }));
        }
    }
    // Single-agent spawn always uses per-agent subdir layout. Spells
    // are the only path that can ask for a shared workspace.
    let layout = WorkspaceLayout::PerAgent {
        root: workspace_root,
    };
    let outcome = spawn_with_bookkeeping(
        &state,
        &req.cli,
        req.role,
        req.model,
        None,
        layout,
        ws.id,
        None,
        resolved_thread_id,
    )
    .await
    .map_err(|(status, msg)| (status, Json(json!({"error": msg}))))?;
    Ok(Json(SpawnAgentResponse {
        agent_id: outcome.agent_id,
        cli: outcome.cli,
        role: outcome.role,
        workspace: outcome.workspace,
        fallback_from: outcome.fallback_from,
        billing_surface: Some(billing_surface_token(outcome.billing_surface)),
    }))
}

/// Outcome of [`spawn_with_bookkeeping`]. Carries the identity bits the
/// HTTP handler needs to build a response **and** a fresh lifecycle
/// subscription so longer-running orchestrators (spells) can await
/// `ShimReady` before injecting bootstrap input.
pub(crate) struct SpawnOutcome {
    pub agent_id: String,
    pub cli: String,
    pub role: String,
    pub workspace: String,
    /// Set when the requested CLI wasn't installed and an installed fallback
    /// was spawned instead: the REQUESTED plugin id (`cli` above is then the
    /// fallback). Billing red line: the fallback filter allows API-billed
    /// engines (e.g. reasonix's `requires_explicit_billing_opt_in == false`),
    /// so every spawn response carries this and the UI can warn loudly instead
    /// of the fallback only leaving a server-side `warn` log.
    pub fallback_from: Option<String>,
    /// Billing surface of the engine that ACTUALLY spawned — paired with
    /// `fallback_from` so the caller can say "按 API 计费" when it matters.
    pub billing_surface: crate::plugins::BillingSurface,
    pub lifecycle_rx: tokio::sync::broadcast::Receiver<LifecycleEvent>,
}

/// Wire token for a plugin's billing surface. swarmx-protocol DTOs carry the
/// surface as a plain string (the protocol crate can't depend on the server
/// crate's BillingSurface enum); reuse the enum's serde kebab-case rename so
/// the token can't drift from the manifest spelling.
fn billing_surface_token(surface: crate::plugins::BillingSurface) -> String {
    serde_json::to_value(surface)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Shared "spawn + register + wire bookkeeping" pipeline used by both
/// POST /api/agent and the spell runner. Identical end state to the
/// previous monolithic handler — only the return path differs.
///
/// `layout` decides where on disk the agent's cwd lands:
/// - `PerAgent { root }` — historic default, agent gets its own
///   `<root>/<agent_id>/` subdir.
/// - `Shared { dir }` — every caller routed through this layout shares
///   the same cwd; used by M6a fullstack-feature spells so FE / BE /
///   Test peers see each other's commits in a single monorepo.
///
/// On success the agent is fully live: PTY pumping, registry insert
/// done, swarm inbox registered, SQLite + ws/swarm fan-out task spawned,
/// recording file open and finalize-watcher scheduled.
/// Hard ceiling on concurrent **live** agents — the fork-bomb guard (F4).
/// Counts the in-memory registry (killed agents are removed from it), so it's
/// a true concurrency bound. Env override `SWARMX_MAX_LIVE_AGENTS` for
/// bigger hosts; 0 / unparseable falls back to the default.
fn max_live_agents() -> usize {
    std::env::var("SWARMX_MAX_LIVE_AGENTS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(256)
}

/// Max delegation depth (orchestrator → worker → worker → …). Env override
/// `SWARMX_MAX_SPAWN_DEPTH`; 0 / unparseable falls back to the default.
fn max_spawn_depth() -> usize {
    std::env::var("SWARMX_MAX_SPAWN_DEPTH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(6)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_with_bookkeeping(
    state: &AppState,
    cli: &str,
    role: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    layout: WorkspaceLayout,
    workspace_id: String,
    spell_run_id: Option<String>,
    thread_id: Option<String>,
) -> Result<SpawnOutcome, (StatusCode, String)> {
    // Fork-bomb guard (F4): a runaway/looping orchestrator — or a worker it
    // spawned — calling swarm_spawn_worker can otherwise fork unbounded real
    // CLI processes (each launched with --dangerously-skip-permissions),
    // exhausting PTYs / RAM / file descriptors and burning API budget. Cap the
    // TOTAL live agents here, the single chokepoint shared by /api/agent,
    // /api/worker and run_spell, so every spawn path is bounded. The auto-kill
    // reaper keeps well-behaved swarms far below this.
    let live = state.registry.list().len();
    let cap = max_live_agents();
    if live >= cap {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "live-agent cap reached ({live}/{cap}); refusing to spawn. \
                 Finish or kill an agent first, or raise SWARMX_MAX_LIVE_AGENTS."
            ),
        ));
    }

    // Budget brake: while the workspace's estimated all-time spend sits at or
    // above its cap, fail EVERY spawn path (here = /api/agent, /api/worker,
    // run_spell, fusion/merge auto-spawns) with an honest, actionable error —
    // a paused-forever spawn would look like a hang. Fail-open on lookup
    // failure (see budget.rs): the brake is soft, a storage hiccup must not
    // brick spawning.
    if let Some(msg) = crate::budget::exceeded_error_for_workspace(&state.store, &workspace_id).await
    {
        return Err((StatusCode::PAYMENT_REQUIRED, msg));
    }

    let (selected_plugin, fallback_from) = select_spawn_plugin(&state.plugins, cli)?;
    if let Some(requested) = fallback_from {
        tracing::warn!(
            requested_cli = %requested.id,
            requested_binary = %requested.binary,
            fallback_cli = %selected_plugin.id,
            fallback_binary = %selected_plugin.binary,
            "requested CLI binary unavailable; falling back to installed provider"
        );
    }
    // Carry the fallback fact onto the outcome so every spawn response
    // (/api/agent, /api/worker, run_spell) can surface it to the user — the
    // warn above alone left a paid-API engine (e.g. reasonix) running in
    // disguise when the user asked for claude.
    let fallback_from_id = fallback_from.map(|p| p.id.clone());
    let plugin: CliPlugin = selected_plugin.clone();
    let actual_cli = plugin.id.as_str();

    // F1: resolve the requested abstract tier into the concrete model for THIS
    // cli, using the per-CLI model config. claude keeps its alias ("sonnet" →
    // "sonnet"); codex gets the user's mapped model or — if unmapped — its own
    // default (no bare "sonnet" forwarded to a custom provider → no 503). A raw
    // model id passes through verbatim. This is the single chokepoint for all
    // three spawn paths (/api/agent, /api/worker, run_spell).
    let model = {
        let resolved = state
            .models
            .read()
            .await
            .resolve(actual_cli, model.as_deref());
        if resolved != model {
            tracing::info!(cli = %actual_cli, from = ?model, to = ?resolved, "model tier resolved per-CLI");
        }
        resolved
    };

    // Reasoning effort: the per-direction `thread.reasoning_effort` arrived as
    // `reasoning` and wins; otherwise fall back to the per-CLI global default
    // (模型 settings). None ⇒ the model's own default (no effort flag). Single
    // chokepoint so every spawn path inherits the global default.
    let reasoning = match reasoning {
        Some(r) => Some(r),
        None => state.models.read().await.effort_for(actual_cli),
    };

    let spawned_at = now_ms();

    // Mint the recording id + path up front so spawn_agent can hand the
    // pump a writer handle. If the recorder fails to open, we still spawn
    // the agent (recording is best-effort, not load-bearing for M3).
    let recording_id = format!("rec-{}", &Uuid::new_v4().to_string()[..12]);
    let recording_path = state.recordings_root.join(format!("{}.cast", recording_id));
    let recorder = match Recorder::start(RecorderConfig {
        agent_id: String::new(), // filled in by the writer config; informational only
        cols: 120,
        rows: 32,
        started_at_ms: spawned_at,
        file_path: recording_path.clone(),
        max_bytes: swarmx_recorder::DEFAULT_MAX_CAST_BYTES,
    })
    .await
    {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(?e, "recorder open failed; spawning agent without recording");
            None
        }
    };
    let recorder_handle = recorder.as_ref().map(|r| r.handle());

    let result = match spawn_agent(
        &plugin,
        role,
        model,
        reasoning,
        &layout,
        &state.shim_path,
        &state.mcp_bin,
        &state.server_url,
        recorder_handle,
    ) {
        Ok(r) => r,
        Err(e) => {
            // Spawn failed AFTER the recorder minted its .cast up front: no PTY
            // pump took the writer and no agent row will ever reference it —
            // the file is an orphan. Best-effort remove; tolerate failure (a
            // header-only leftover is harmless, and a still-open handle can
            // refuse the unlink on Windows) — warn and take the original error.
            if recorder.is_some() {
                if let Err(re) = tokio::fs::remove_file(&recording_path).await {
                    tracing::warn!(?re, path = %recording_path.display(), "spawn failed; could not remove orphan .cast");
                }
            }
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    let agent_id = result.agent_id.clone();

    // Persist the spawn. PTY is already live; on store failure we log and
    // keep the in-memory agent — the user can still attach and use it.
    if let Err(e) = state
        .store
        .record_agent_spawn(NewAgent {
            id: agent_id.clone(),
            cli: result.slot.cli.clone(),
            role: result.slot.role.clone(),
            workspace: result.slot.workspace.clone(),
            spawned_at,
            workspace_id: Some(workspace_id.clone()),
            spell_run_id: spell_run_id.clone(),
            // Direction this agent belongs to. `None` = the workspace's main
            // thread (resolved by callers; legacy/pre-thread spawns also None).
            thread_id: thread_id.clone(),
        })
        .await
    {
        tracing::warn!(?e, agent = %agent_id, "record_agent_spawn failed");
    }

    drop(state.swarm.register_agent(agent_id.clone()));

    state.swarm.publish_event(SwarmEvent::AgentState {
        agent_id: agent_id.clone(),
        state: AgentState::Spawning,
    });

    // Subscribe twice: once for our own internal SQLite+swarm fan-out
    // task, and a fresh receiver for the caller (so e.g. the spell runner
    // can `await` ShimReady without racing the bookkeeping task).
    let lifecycle_rx_for_caller = result.slot.lifecycle_tx.subscribe();
    {
        let mut lifecycle_rx = result.slot.lifecycle_tx.subscribe();
        let store = state.store.clone();
        let swarm = state.swarm.clone();
        let agent_for_task = agent_id.clone();
        // Engine id (= plugin id) for real-usage write-back into the probe cache.
        let engine_for_task = result.slot.cli.clone();
        tokio::spawn(async move {
            loop {
                match lifecycle_rx.recv().await {
                    Ok(LifecycleEvent::ShimReady) => {
                        let at = now_ms();
                        if let Err(e) = store.record_shim_ready(agent_for_task.clone(), at).await {
                            tracing::warn!(?e, agent = %agent_for_task, "record_shim_ready failed");
                        }
                        swarm.publish_event(SwarmEvent::AgentState {
                            agent_id: agent_for_task.clone(),
                            state: AgentState::Ready,
                        });
                        swarm.publish_event(SwarmEvent::AgentStage {
                            agent_id: agent_for_task.clone(),
                            stage: "shim_ready".into(),
                            at,
                        });
                        // Real-usage write-back: this engine just came up over the
                        // production spawn path — record it Usable in the probe
                        // cache so the readiness UI reflects it without a manual
                        // probe (same launch-only bar the probe uses).
                        crate::engine_probe::record_live_verdict(
                            &engine_for_task,
                            crate::engine_probe::ProbeState::Usable,
                            None,
                            None,
                            "live-ready",
                        );
                        // First-response watchdog. A "ready" shim only means the
                        // PTY came up — NOT that the agent can do work. If it
                        // produces no message, no tool activity and no token
                        // usage within the window, it's wedged (an auth prompt
                        // we didn't needle, a hung hook, an MCP that never
                        // settled) — flip it to Error so the UI shows an honest
                        // failure card instead of a green dot + "暂无消息". This
                        // is the only liveness check that covers the orchestrator
                        // (the transcript tailer doesn't watch it). Detached so
                        // it never blocks the lifecycle loop.
                        let store_wd = store.clone();
                        let swarm_wd = swarm.clone();
                        let agent_wd = agent_for_task.clone();
                        let watchdog_ms = first_response_watchdog_ms(&engine_for_task);
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(watchdog_ms)).await;
                            match store_wd.agent_silent_since_ready(agent_wd.clone()).await {
                                Ok(true) => {
                                    let reason = "启动后无响应（可能未登录或卡住）".to_string();
                                    let at = now_ms();
                                    if let Err(e) = store_wd
                                        .record_agent_error(
                                            agent_wd.clone(),
                                            reason.clone(),
                                            "watchdog",
                                            at,
                                        )
                                        .await
                                    {
                                        tracing::warn!(?e, agent = %agent_wd, "watchdog record_agent_error failed");
                                    }
                                    tracing::warn!(agent = %agent_wd, "first-response watchdog fired: no message/activity/usage after ready");
                                    swarm_wd.publish_event(SwarmEvent::AgentState {
                                        agent_id: agent_wd.clone(),
                                        state: AgentState::Error,
                                    });
                                    swarm_wd.publish_event(SwarmEvent::AgentActivity {
                                        agent_id: agent_wd.clone(),
                                        kind: "system".to_string(),
                                        label: reason,
                                        phase: "error".to_string(),
                                        seq: 0,
                                        duration_ms: None,
                                        at,
                                    });
                                }
                                // Healthy (spoke / used tokens) or already
                                // handled (killed / errored) — nothing to do.
                                Ok(false) => {}
                                Err(e) => {
                                    tracing::warn!(?e, agent = %agent_wd, "watchdog probe failed")
                                }
                            }
                        });
                    }
                    Ok(LifecycleEvent::ShimExit(code)) => {
                        let at = now_ms();
                        // Lifecycle breadcrumb: the agent's CLI process ended —
                        // this is the moment a pending "正在响应" bubble for it
                        // will vanish on the client. code==0 → neutral Exited
                        // (no failure card today); code!=0 → Error (red card).
                        tracing::info!(
                            agent = %agent_for_task,
                            code,
                            terminal = if code == 0 { "exited" } else { "error" },
                            "shim exited — agent turn ends"
                        );
                        if let Err(e) = store
                            .record_shim_exit(agent_for_task.clone(), code, at)
                            .await
                        {
                            tracing::warn!(?e, agent = %agent_for_task, "record_shim_exit failed");
                        }
                        // Non-zero exit = abnormal death → Error (the UI
                        // surfaces it red, sorted to top). Clean exit → Exited.
                        // Intentional kills also exit non-zero, but those rows
                        // carry `killed_at`, which the UI prioritizes over this.
                        let next = if code == 0 {
                            AgentState::Exited
                        } else {
                            AgentState::Error
                        };
                        swarm.publish_event(SwarmEvent::AgentState {
                            agent_id: agent_for_task.clone(),
                            state: next,
                        });
                    }
                    Ok(LifecycleEvent::HealthFail { reason, kind }) => {
                        // The CLI is alive but told us (via a PTY banner the
                        // pump's HealthScanner matched) that it can't actually
                        // work — not logged in / rate limited / invalid key.
                        // Flip to Error (red, sorted to top) and ride the
                        // human-facing detail on a system AgentActivity so the
                        // UI can render an honest, actionable failure card in
                        // place of the fake "online" dot + "暂无消息".
                        let at = now_ms();
                        if let Err(e) = store
                            .record_agent_error(agent_for_task.clone(), reason.clone(), &kind, at)
                            .await
                        {
                            tracing::warn!(?e, agent = %agent_for_task, "record_agent_error failed");
                        }
                        // Real-usage write-back: a live agent's auth/quota banner
                        // is direct evidence the engine can't run as-is. "auth" →
                        // NeedsLogin (UI offers a login command); anything else →
                        // NotUsable.
                        crate::engine_probe::record_live_verdict(
                            &engine_for_task,
                            if kind == "auth" {
                                crate::engine_probe::ProbeState::NeedsLogin
                            } else {
                                crate::engine_probe::ProbeState::NotUsable
                            },
                            Some(reason.clone()),
                            Some(kind.clone()),
                            "live-health-needle",
                        );
                        swarm.publish_event(SwarmEvent::AgentState {
                            agent_id: agent_for_task.clone(),
                            state: AgentState::Error,
                        });
                        swarm.publish_event(SwarmEvent::AgentActivity {
                            agent_id: agent_for_task.clone(),
                            kind: "system".to_string(),
                            label: reason,
                            phase: "error".to_string(),
                            seq: 0,
                            duration_ms: None,
                            at,
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(agent = %agent_for_task, lagged = n, "lifecycle subscriber lagged");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::debug!(agent = %agent_for_task, "lifecycle subscriber closed");
        });
    }

    if let Some(rec) = recorder {
        let new_rec = NewRecording {
            id: recording_id.clone(),
            agent_id: agent_id.clone(),
            path: recording_path.to_string_lossy().into_owned(),
            started_at: spawned_at,
            cols: 120,
            rows: 32,
        };
        if let Err(e) = state.store.record_recording_start(new_rec).await {
            tracing::warn!(?e, agent = %agent_id, "record_recording_start failed");
        }
        let store = state.store.clone();
        let rec_id_for_task = recording_id.clone();
        let agent_for_task = agent_id.clone();
        tokio::spawn(async move {
            match rec.wait_finalize().await {
                Ok(fin) => {
                    if let Err(e) = store
                        .record_recording_finalize(
                            rec_id_for_task,
                            fin.finalized_at_ms,
                            fin.duration_ms,
                            fin.last_seq,
                        )
                        .await
                    {
                        tracing::warn!(?e, agent = %agent_for_task, "record_recording_finalize failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(?e, agent = %agent_for_task, "recorder wait_finalize failed");
                }
            }
        });
    }

    // Tail this worker's CLI session transcript to surface tool-level activity
    // in the UI — zero cost to the worker (we read the JSONL it already writes,
    // never hook or slow it). No-op for a CLI with no known transcript format.
    crate::transcript::spawn_tailer(
        state.swarm.clone(),
        state.store.clone(),
        state.registry.clone(),
        agent_id.clone(),
        result.slot.cli.clone(),
        std::path::PathBuf::from(&result.slot.workspace),
        result.transcript_session_id.clone(),
    );

    let outcome = SpawnOutcome {
        agent_id: agent_id.clone(),
        cli: result.slot.cli.clone(),
        role: result.slot.role.clone(),
        workspace: result.slot.workspace.clone(),
        fallback_from: fallback_from_id,
        billing_surface: plugin.billing_surface,
        lifecycle_rx: lifecycle_rx_for_caller,
    };
    state.registry.insert(agent_id, result.slot);
    Ok(outcome)
}

pub async fn list_agents(State(state): State<AppState>) -> impl IntoResponse {
    // Role-registry handoff lookup. Empty handoff_signal means the role
    // doesn't produce a key (orchestrator, etc.). Cloned locally so we
    // don't hold the registry borrow across async boundaries.
    let role_handoff: std::collections::HashMap<String, String> = state
        .roles
        .list()
        .into_iter()
        .map(|r| (r.manifest.id.clone(), r.manifest.handoff_signal.clone()))
        .collect();

    let handoff_for =
        |role: &str| -> String { role_handoff.get(role).cloned().unwrap_or_default() };
    // depends_on used to come from `wake_subs`, but that table is the
    // INTERNAL "wake me when this key lands" registration — Magentic-One's
    // append_wake_sub bug-#48 fix made the orchestrator subscribe to every
    // worker's handoff_signal, which then leaked into the DAG as a fake
    // "orchestrator depends on worker" edge. Two different concepts had
    // ended up sharing one field. Task-graph depends_on is now read
    // strictly from the `workers` row backfill below; orchestrator (not
    // in workers table) gets empty deps and renders as a DAG root, which
    // matches the user's mental model.
    let depends_for = |_agent_id: &str| -> Vec<String> { Vec::new() };

    // Build a snapshot of the live in-memory registry first — for live
    // agents the in-memory `Lifecycle` is the source of truth (it tracks
    // OSC markers that may not yet be persisted to SQLite).
    let mut live: std::collections::HashMap<String, AgentInfo> = std::collections::HashMap::new();
    for (id, slot) in state.registry.list() {
        let slot = slot.lock();
        let lc = *slot.lifecycle.lock();
        // Converge a process that died WITHOUT emitting an OSC exit marker
        // (SIGKILL / OOM): the in-memory Lifecycle would otherwise keep reporting
        // shim_ready=true, shim_exit=None — UI shows a green/ready agent — until
        // the 5s reaper sweep catches it. A deterministic is_alive() (waitpid)
        // here collapses that "lying green" window to zero: a dead-but-unmarked
        // agent reports an abnormal exit immediately.
        let shim_exit = match lc.shim_exit {
            existing @ Some(_) => existing,
            None if !slot.is_alive() => Some(-1),
            None => None,
        };
        let depends_on = depends_for(&id);
        let handoff_signal = handoff_for(&slot.role);
        let paused = slot.paused.load(std::sync::atomic::Ordering::Relaxed);
        live.insert(
            id.clone(),
            AgentInfo {
                agent_id: id,
                cli: slot.cli.clone(),
                role: slot.role.clone(),
                workspace: slot.workspace.clone(),
                shim_ready: lc.shim_ready,
                shim_exit,
                killed_at: None,
                spawned_at: None,
                depends_on,
                handoff_signal,
                // Step 1: AgentSlot doesn't carry workspace_id yet (Step 3
                // wires that in). For live entries the SQLite row is the
                // authoritative source and we backfill from it below.
                workspace_id: None,
                spell_run_id: None,
                thread_id: None, // backfilled from SQLite below
                // parent_agent_id is derived from the spell_runs table after
                // the SQLite union below — fills in once spell_run_id is set.
                parent_agent_id: None,
                paused,
                // Backfilled from the SQLite row in the union below (the live
                // registry slot doesn't carry it).
                last_activity_at: None,
                last_error: None,
                last_error_kind: None,
                last_error_at: None,
                // Backfilled in the batch `.error` pass after the SQLite union.
                handoff_failed: false,
                handoff_missing: false,
                // Backfilled in the stalled pass after the SQLite union.
                stalled: false,
            },
        );
    }

    // Union with SQLite history so a freshly-started server can still tell
    // the UI about agents from prior runs. Live entries win — they keep
    // their `shim_ready`/`shim_exit` derived from the in-memory lifecycle.
    let mut items: Vec<AgentInfo> = Vec::new();
    match state.store.list_agents().await {
        Ok(rows) => {
            for row in rows {
                if let Some(mut info) = live.remove(&row.id) {
                    // Backfill the timestamps + workspace lineage from
                    // SQLite but keep the live lifecycle snapshot.
                    info.spawned_at = Some(row.spawned_at);
                    info.workspace_id = row.workspace_id;
                    info.spell_run_id = row.spell_run_id;
                    info.thread_id = row.thread_id;
                    info.last_activity_at = row.last_activity_at;
                    info.last_error = row.last_error;
                    info.last_error_kind = row.last_error_kind;
                    info.last_error_at = row.last_error_at;
                    items.push(info);
                } else {
                    // Historical row: depends_on is empty (subscription
                    // was unregistered at kill); handoff_signal still
                    // computed from the saved role so the graph can
                    // place the node even when its wake-state is gone.
                    let handoff_signal = handoff_for(&row.role);
                    items.push(AgentInfo {
                        agent_id: row.id,
                        cli: row.cli,
                        role: row.role,
                        workspace: row.workspace,
                        shim_ready: row.shim_ready_at.is_some(),
                        shim_exit: row.shim_exit_code,
                        killed_at: row.killed_at,
                        spawned_at: Some(row.spawned_at),
                        depends_on: Vec::new(),
                        handoff_signal,
                        workspace_id: row.workspace_id,
                        spell_run_id: row.spell_run_id,
                        thread_id: row.thread_id,
                        parent_agent_id: None,
                        paused: false,
                        last_activity_at: row.last_activity_at,
                        last_error: row.last_error,
                        last_error_kind: row.last_error_kind,
                        last_error_at: row.last_error_at,
                        // Backfilled in the batch `.error` pass below.
                        handoff_failed: false,
                        handoff_missing: false,
                        // Historical rows are never alive, so the stalled pass
                        // below never flags them — but default it explicitly.
                        stalled: false,
                    });
                }
            }
        }
        Err(e) => {
            tracing::warn!(?e, "list_agents: store.list_agents failed; live-only view");
        }
    }
    // Any live entries that weren't in the store (shouldn't happen, but
    // be defensive) get appended at the end.
    items.extend(live.into_values());

    // Derive parent_agent_id from workers.parent_agent_id (Magentic-One
    // 重构后业务 agent 全部走 workers 表)。orchestrator 本身不在 workers
    // 表里 — 它是 init spell 拉的,parent 为 None(树根),符合"用户的
    // 助手"语义。一次批量 IN 查询,N+1 不允许。
    let worker_ids: Vec<String> = items.iter().map(|it| it.agent_id.clone()).collect();
    if !worker_ids.is_empty() {
        match state.store.list_workers_by_ids(worker_ids).await {
            Ok(by_id) => {
                for it in items.iter_mut() {
                    if let Some(w) = by_id.get(&it.agent_id) {
                        it.parent_agent_id = Some(w.parent_agent_id.clone());
                        // Magentic-One workers carry their handoff_signal +
                        // depends_on on the `workers` row, NOT on a role
                        // manifest (their role_label is ad-hoc, picked by
                        // the orchestrator). Backfill both so the DAG view
                        // can render the dashed waiting edges — without
                        // this, AgentInfo.handoff_signal stays empty (the
                        // role lookup above misses), depends_on stays
                        // empty (only live registry knew it), and
                        // deriveEdges falls back to "no producer" → no
                        // dashed line ever drawn.
                        if it.handoff_signal.is_empty() && !w.handoff_signal.is_empty() {
                            it.handoff_signal = w.handoff_signal.clone();
                        }
                        if it.depends_on.is_empty() && !w.depends_on_json.is_empty() {
                            if let Ok(parsed) =
                                serde_json::from_str::<Vec<String>>(&w.depends_on_json)
                            {
                                it.depends_on = parsed;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    "list_agents: list_workers_by_ids failed; parent edges omitted"
                );
            }
        }
    }

    // Failed-handoff detection: a worker that aborted writes
    // `<handoff_signal>.error` instead of its success key. `handoff_signal`
    // (now backfilled from the workers row above) always holds the DECLARED
    // success key, so without this pass a failed worker looks identical to a
    // delivered one in the DAG.
    //
    // IMPORTANT: a failure only counts when the `.error` key exists AND the
    // success key does NOT. When a worker aborts but the orchestrator (or a
    // retry) then writes the success key as a fallback, BOTH keys end up on
    // the blackboard — that's a net SUCCESS, not a failure, so we must not
    // flag it. Querying both variants in one batch lets us require
    // `error_present && !success_present`.
    let mut probe_keys: Vec<String> = Vec::new();
    for it in items.iter().filter(|it| !it.handoff_signal.is_empty()) {
        probe_keys.push(it.handoff_signal.clone());
        probe_keys.push(format!("{}.error", it.handoff_signal));
    }
    if !probe_keys.is_empty() {
        match state.store.blackboard_paths_present(probe_keys).await {
            Ok(present) => {
                for it in items.iter_mut() {
                    if it.handoff_signal.is_empty() {
                        continue;
                    }
                    let error_present = present.contains(&format!("{}.error", it.handoff_signal));
                    let success_present = present.contains(&it.handoff_signal);
                    // Failed only if it errored and was never (fallback-)delivered.
                    it.handoff_failed = error_present && !success_present;
                    // Premature/silent handoff: the agent has EXITED but neither
                    // the success key nor the `.error` marker exists — it left
                    // without delivering anything. Live agents are exempt (they
                    // may still deliver). This is the failure the chat surfaces
                    // because, unlike handoff_failed, there's no `.error` trail.
                    let exited = it.killed_at.is_some() || it.shim_exit.is_some();
                    it.handoff_missing = exited && !success_present && !error_present;
                }
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    "list_agents: blackboard_paths_present failed; handoff_failed flags omitted"
                );
            }
        }
    }

    // Stalled detection for the NeedsYou inbox's 「该醒没醒」lane: an ALIVE
    // agent (shim ready, not exited, not killed, not paused) with unread mail
    // older than STALLED_UNREAD_SILENCE_MS and no activity in that window —
    // the precise shape of a broken wake chain. Deliberately NOT the 0.3.0
    // suspected-stuck heuristic (misflagged a working codex; zero-false-
    // positive is the bar, see STALLED_UNREAD_SILENCE_MS).
    //
    // Paused agents are excluded here (the store can't see the in-memory
    // flag): a paused agent's undelivered wake is DELIBERATE (the operator
    // turned auto-wake off), not a broken chain.
    let now = now_ms();
    let stalled_candidates: Vec<String> = items
        .iter()
        .filter(|it| {
            it.shim_ready && it.shim_exit.is_none() && it.killed_at.is_none() && !it.paused
        })
        .map(|it| it.agent_id.clone())
        .collect();
    if !stalled_candidates.is_empty() {
        match state
            .store
            .stalled_agents_with_unread(stalled_candidates, now, STALLED_UNREAD_SILENCE_MS)
            .await
        {
            Ok(flagged) => {
                if !flagged.is_empty() {
                    let cutoff = now - STALLED_UNREAD_SILENCE_MS;
                    for it in items.iter_mut() {
                        if !flagged.contains(&it.agent_id) {
                            continue;
                        }
                        // Live-activity guard: the persisted `last_activity_at`
                        // is only stamped by the transcript tailer (claude/
                        // codex) — opencode/reasonix/zulu report tool activity
                        // over the ingress POST, which feeds the in-memory ring
                        // (and WS) but NOT that column. Without this check a
                        // mid-turn opencode worker that received mail minutes
                        // ago would read as "silent" — the exact 0.3.0 false
                        // positive. Fresh ring activity ⇒ alive and working.
                        let live_active = state
                            .swarm
                            .recent_activity(&it.agent_id)
                            .iter()
                            .any(|rec| rec.at > cutoff);
                        it.stalled = !live_active;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    "list_agents: stalled_agents_with_unread failed; stalled flags omitted"
                );
            }
        }
    }

    Json(items)
}

/// Full agent teardown, shared by REST `DELETE /api/agent/:id` and the WS
/// `ClientControl::Kill` path so the two can't diverge (F1): kill the PTY, drop
/// the in-memory inbox, unregister the wake subscription, persist the kill, and
/// broadcast `Exited`. Returns `true` if the agent existed (caller maps to
/// 200 vs 404). NOTE: the broadcast `Exited` does NOT bypass the WakeCoordinator
/// — its AgentState arm (wake.rs) routes `Exited` AND `Error` alike into
/// `handle_agent_exit`, which unregisters the exit_key and writes the
/// `<handoff>.error` alias for dependents when no fresh signal landed. What
/// teardown itself skips is only the farewell message and the auto-kill grace.
pub(crate) async fn teardown_agent(state: &AppState, agent_id: &str) -> bool {
    match state.registry.remove(agent_id) {
        Some(slot) => {
            // Lifecycle breadcrumb: an explicit teardown (DELETE /api/agent,
            // WS Kill, model-switch restart, bootstrap-replace). Sets killed_at
            // and broadcasts a NEUTRAL Exited — so if this agent had an
            // unanswered user message, its pending bubble vanishes with no
            // failure card. Logged so "正在…然后突然没了" reports can be traced
            // to who/when tore the captain down.
            tracing::info!(agent = %agent_id, "teardown_agent: killing agent (killed_at, neutral Exited)");
            // Offload the blocking kill (drops the slot lock first, runs on the
            // blocking pool) so a DELETE — or a direction-delete tearing down N
            // workers serially — never freezes a tokio worker or the slot lock.
            crate::registry::offload_kill(&slot).await;
            // Drop the in-memory inbox before persisting the kill so any
            // in-flight send_message sees "no inbox" rather than racing
            // against a half-torn-down agent.
            state.swarm.unregister_agent(agent_id);
            // M6b: tear down the wake subscription too so we don't try
            // to inject into a registry slot that's about to be dropped.
            crate::wake::unregister_wake_subs(&state.wake_subs, agent_id).await;
            if let Err(e) = state
                .store
                .record_agent_kill(agent_id.to_string(), now_ms())
                .await
            {
                tracing::warn!(?e, agent = %agent_id, "record_agent_kill failed");
            }
            state.swarm.publish_event(SwarmEvent::AgentState {
                agent_id: agent_id.to_string(),
                state: AgentState::Exited,
            });
            true
        }
        None => false,
    }
}

pub async fn kill(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    if teardown_agent(&state, &agent_id).await {
        (StatusCode::OK, Json(json!({"ok": true})))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("agent {agent_id} not found")})),
        )
    }
}

/// M6e: operator-triggered manual wake. The UI's ⚡ button posts here
/// when the operator believes an agent has missed its natural wake or
/// is stuck. Delivery is the same mailbox + PTY-kick pair that the
/// event-driven wake uses, with a body that says "manual wake from
/// operator" so the recipient understands the context. Returns 404 if
/// the agent isn't in the registry (already exited, never spawned).
pub async fn wake_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    if state.registry.get(&agent_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("agent {agent_id} not found")})),
        );
    }
    // Budget brake: a manual wake is a new turn delivery — while the
    // workspace's brake is on it would burn more of the capped budget.
    if let Some(msg) = crate::budget::exceeded_error_for_agent(&state.store, &agent_id).await {
        return (StatusCode::PAYMENT_REQUIRED, Json(json!({"error": msg})));
    }
    match crate::wake::deliver_manual_wake(
        &state.swarm,
        &state.registry,
        &state.server_url,
        &agent_id,
    )
    .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => {
            tracing::warn!(?e, agent = %agent_id, "manual wake failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    }
}

/// `GET /api/agent/:id/activity` — recent tool-level activity for an agent,
/// served from the transcript tailer's in-memory ring. Backfills the drawer's
/// Activity tab on a cold open / after a remount, where the forward-only
/// `AgentActivity` WS stream shows nothing for an agent that already did its
/// work. Always 200; empty `[]` for agents we don't tail or that haven't acted
/// yet. Oldest-first, same `seq` space as the live stream so the UI merges them.
pub async fn agent_activity(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    // P1: the persisted table (survives cold load / reconnect / restart) is the
    // base; overlay the live in-memory ring, which is fresher for an agent
    // acting right now and may hold a step not yet flushed. Merge by seq (ring
    // wins), oldest-first — the BTreeMap keeps seq order, the same space the
    // live WS uses so the UI merges them seamlessly.
    let persisted = state
        .swarm
        .store()
        .recent_agent_activities(&agent_id, 200)
        .await
        .unwrap_or_default();
    let mut by_seq: std::collections::BTreeMap<u32, AgentActivityRecord> =
        std::collections::BTreeMap::new();
    for row in persisted {
        by_seq.insert(
            row.seq,
            AgentActivityRecord {
                agent_id: row.agent_id,
                kind: row.kind,
                label: row.label,
                phase: row.phase,
                seq: row.seq,
                duration_ms: row.duration_ms,
                at: row.at,
            },
        );
    }
    for rec in state.swarm.recent_activity(&agent_id) {
        by_seq.insert(rec.seq, rec);
    }
    Json(by_seq.into_values().collect::<Vec<_>>())
}

/// Body for `POST /api/agent/:id/activity`.
#[derive(Debug, serde::Deserialize)]
pub struct AgentActivityIngress {
    /// "running" (tool started) | "ok" (finished) | "error" (failed).
    pub phase: String,
    /// One-line tool label, e.g. `swarm_write_blackboard e2e/opencode-ok`.
    pub label: String,
    /// Per-turn sequence number pairing a `running` row with its later
    /// `ok`/`error` (same seq space as the transcript tailer / live WS).
    pub seq: u32,
    /// Optional wall-clock duration (ms), set on the `ok`/`error` row.
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

/// `POST /api/agent/:id/activity` — tool-activity ingress for engines that
/// CANNOT be transcript-tailed. claude/codex write a single append-only JSONL
/// session file the tailer byte-follows; opencode instead writes a tree of
/// per-message/part JSON files (not tailable). So the swarmx opencode plugin
/// (cli-plugins/opencode/swarmx-wake.js) POSTs each tool call's start/finish
/// here, and we feed it into the SAME pipeline the tailer uses (ring + SQLite +
/// thought-trace derivation + WS), so opencode tool steps show in the UI just
/// like claude/codex. Always cheap + best-effort; an unknown agent → 404.
pub async fn post_agent_activity(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<AgentActivityIngress>,
) -> impl IntoResponse {
    if state.registry.get(&agent_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("agent {agent_id} not found")})),
        );
    }
    let phase = match body.phase.as_str() {
        "running" | "ok" | "error" => body.phase,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid phase '{other}' (running|ok|error)")})),
            );
        }
    };
    crate::transcript::emit_activity(
        &state.swarm,
        &agent_id,
        &phase,
        body.label,
        body.seq,
        body.duration_ms,
        now_ms(),
    );
    (StatusCode::OK, Json(json!({"ok": true})))
}

// ────────────────────────────────────────────────────────────────────────
// Spawn ad-hoc worker (Magentic-One 重构): orchestrator 通过 MCP
// swarm_spawn_worker 拉一个临时 worker。绕过 spell + role,worker 的
// prompt / handoff_signal / depends_on 全部来自请求体,server 复用
// spawn_with_bookkeeping 完整拉 PTY,然后:
//   1. workers 表写一行(留档 + DAG parent 派生)
//   2. WakeCoordinator 注册 wake_subs + exit_keys
//   3. 后台等 ShimReady 后注入 system_prompt(跟 run_spell 同款 paste+\r)
// 跟 run_spell 的区别只是"不解析 spell / 不查 role registry / 不挂 spell_run"。
// ────────────────────────────────────────────────────────────────────────

// BootstrapCtx + spawn_bootstrap_inject live in `crate::agent_lifecycle`
// (ShimReady → mcp-ready → readiness gate → TurnDelivery / PTY paste).
pub(crate) use crate::agent_lifecycle::{spawn_bootstrap_inject, BootstrapCtx};

pub async fn spawn_worker(
    State(state): State<AppState>,
    Json(req): Json<SpawnWorkerRequest>,
) -> Result<Json<SpawnWorkerResponse>, (StatusCode, Json<serde_json::Value>)> {
    if req.role.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing role (pass a registry slug; see swarm_list_roles)"})),
        ));
    }
    if req.system_prompt.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing system_prompt"})),
        ));
    }
    if req.workspace_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing workspace_id"})),
        ));
    }
    if req.caller_agent_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing caller_agent_id"})),
        ));
    }

    // Resolve workspace cwd. spawn_worker only supports Shared layout
    // because the orchestrator-and-workers model assumes everyone works
    // in the same monorepo / project dir (跟 fullstack-feature 一致)。
    let ws = state
        .store
        .get_workspace_by_id(req.workspace_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("workspace lookup failed: {e}")})),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown workspace_id: {}", req.workspace_id)})),
            )
        })?;

    // SECURITY: authorize the caller. `caller_agent_id` is the context this
    // spawn inherits (thread/cwd/blackboard namespace), so an unvalidated id
    // would let a caller borrow ANY workspace/thread as context — a cross-
    // workspace escalation. Require that the agent (a) exists and (b) belongs to
    // the SAME workspace we're spawning into. `get_workspace_id_for_agent`
    // returns `None` both when the row is absent and when its `workspace_id` is
    // NULL; both are unauthorized here (an agent with no workspace cannot
    // authorize a spawn into a specific one), so collapsing them is correct.
    let caller_ws = state
        .store
        .get_workspace_id_for_agent(req.caller_agent_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("caller agent lookup failed: {e}")})),
            )
        })?;
    match caller_ws {
        Some(ws_id) if ws_id == req.workspace_id => {}
        // Don't leak whether the agent exists vs. lives elsewhere — same 403.
        _ => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "caller_agent_id is not a member of this workspace"
                })),
            ));
        }
    }

    // Inherit the caller's direction (thread): a worker runs in the same
    // thread — and thus the same worktree cwd — as the orchestrator/worker
    // that delegated it, so siblings on one direction don't clobber another
    // direction's working tree. A genuine `None` (caller has no thread) = the
    // workspace's main thread, whose cwd is the workspace cwd.
    // A hard lookup error must NOT silently fall back to the workspace cwd:
    // for an isolated direction that would run file work in the WRONG (shared)
    // tree. So a DB error fails the spawn; only a genuine `None` (caller has no
    // thread) maps to the main thread / workspace cwd.
    let thread_id = state
        .store
        .get_thread_id_for_agent(req.caller_agent_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("thread lookup failed: {e}")})),
            )
        })?;
    // Both the cwd AND the slug: the slug is the blackboard namespace segment
    // (`<workspace_id>/<thread_slug>/…`) that minted handoff keys are scoped by,
    // so producer + consumer keys match within a direction and never collide
    // across directions. Main thread (no row) → slug "main".
    let (thread_cwd, thread_slug, thread_model_tier, thread_reasoning) = match thread_id.as_ref() {
        Some(tid) => match state.store.get_thread(tid.clone()).await {
            Ok(Some(t)) => (t.cwd, t.slug, t.model_tier, t.reasoning_effort),
            // Thread row gone (deleted) → fall back to the main/project cwd.
            Ok(None) => (ws.cwd.clone(), "main".to_string(), None, None),
            // Hard error: don't guess a directory; fail loudly.
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("thread cwd lookup failed: {e}")})),
                ));
            }
        },
        None => (ws.cwd.clone(), "main".to_string(), None, None),
    };
    let layout = WorkspaceLayout::Shared {
        dir: PathBuf::from(&thread_cwd),
    };

    // ── P0: role registry resolution + typed handoff minting ─────────────
    // Effective registry = global (builtin + repo) overlaid by this
    // workspace/direction's project `.swarmx/roles/` (override by slug).
    let mut registry = (*state.roles).clone();
    let project_roles_dir = PathBuf::from(&thread_cwd).join(".swarmx").join("roles");
    if project_roles_dir.is_dir() {
        match crate::roles::RoleRegistry::load_dir(&project_roles_dir) {
            Ok(proj) => registry.overlay(proj),
            Err(e) => {
                tracing::warn!(?e, dir = %project_roles_dir.display(), "project roles overlay failed")
            }
        }
    }

    // Validate the role slug. Unknown → 400 with valid options + did-you-mean.
    let role_slug = req.role.trim().to_string();
    let role = registry.get(&role_slug).cloned().ok_or_else(|| {
        let valid = registry.ids();
        let mut msg = format!("unknown role '{role_slug}'");
        if let Some(s) = closest_match(&role_slug, &valid) {
            msg.push_str(&format!(" — did you mean '{s}'?"));
        }
        msg.push_str(&format!(" valid roles: {valid:?}"));
        (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
    })?;
    let manifest = &role.manifest;

    // Resolve cli/model from the role's defaults unless explicitly overridden.
    let resolved_cli = req
        .cli
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(manifest.default_cli.as_str())
        .to_string();
    if resolved_cli.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": format!("role '{role_slug}' has no default_cli and no cli override was given")}),
            ),
        ));
    }
    // Precedence: explicit spawn request → role's pinned tier → the direction's
    // model_tier (the user's per-direction choice) → none (global default).
    let resolved_model = req
        .model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let t = manifest.default_model_tier.trim();
            (!t.is_empty()).then(|| t.to_string())
        })
        .or_else(|| thread_model_tier.clone());
    let role_label = if manifest.name.trim().is_empty() {
        role_slug.clone()
    } else {
        manifest.name.clone()
    };

    // Effective produces: spawn override → role.produces → ["done"].
    let produces: Vec<String> = if !req.produces.is_empty() {
        req.produces
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if !manifest.produces.is_empty() {
        manifest.produces.clone()
    } else {
        vec!["done".to_string()]
    };

    // W2-1 verify gate (opt-in, default OFF): the role manifest may declare
    // `done_checks` — objective commands the server runs in the worker's cwd
    // after it writes its handoff key, BEFORE the completion is accepted
    // (auto-kill). Empty = no gate, legacy behaviour. Validate every entry
    // against the allowlist NOW — before anything is spawned or persisted —
    // so a bad declaration is a loud 400 the orchestrator can fix, never a
    // silently-swallowed gate (the security boundary lives in verify.rs).
    let done_checks: Vec<String> = manifest
        .done_checks
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    for c in &done_checks {
        if let Err(e) = crate::verify::validate_verify_cmd(c) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("role '{role_slug}' done_checks entry {c:?} rejected: {e}")
                })),
            ));
        }
    }

    // Sibling set drives instance minting (parallel same-role) AND consume
    // binding (wait for currently live producers, else the class key).
    let sibling_workers =
        live_direction_workers(&state, &req.workspace_id, thread_id.as_deref()).await;

    let primary_kind = if produces.iter().any(|k| k == "done") {
        "done"
    } else {
        produces[0].as_str()
    };
    // Claims mutex: two concurrent spawn_worker calls that both listed zero
    // siblings must not both mint the class key.
    let (instance, _handoff_claim, handoff_signal, minted_produces) = {
        let mut claims = lock_claims();
        let reserved_same = claims
            .iter()
            .filter(|c| {
                c.workspace_id == req.workspace_id
                    && c.thread_slug == thread_slug
                    && c.role_slug == role_slug
            })
            .count();
        let live_same = sibling_workers
            .iter()
            .filter(|w| w.role_slug == role_slug)
            .count();
        let instance = crate::roles::pick_handoff_instance(live_same + reserved_same);
        let minted_produces: Vec<String> = produces
            .iter()
            .map(|k| {
                crate::roles::mint_handoff_key(
                    &req.workspace_id,
                    &thread_slug,
                    &role_slug,
                    k,
                    instance.as_deref(),
                )
            })
            .collect();
        let handoff_signal = crate::roles::mint_handoff_key(
            &req.workspace_id,
            &thread_slug,
            &role_slug,
            primary_kind,
            instance.as_deref(),
        );
        claims.push(HandoffClaim {
            workspace_id: req.workspace_id.clone(),
            thread_slug: thread_slug.clone(),
            role_slug: role_slug.clone(),
            handoff_signal: handoff_signal.clone(),
        });
        (
            instance,
            HandoffReservationGuard {
                key: handoff_signal.clone(),
            },
            handoff_signal,
            minted_produces,
        )
    };
    let error_key = format!("{handoff_signal}.error");
    let progress_key = crate::roles::mint_progress_key(
        &req.workspace_id,
        &thread_slug,
        &role_slug,
        instance.as_deref(),
    );

    let mut live_producers: Vec<crate::roles::LiveProducer> = sibling_workers
        .iter()
        .filter(|w| !w.role_slug.is_empty() && !w.handoff_signal.is_empty())
        .map(|w| crate::roles::LiveProducer {
            role_slug: w.role_slug.clone(),
            handoff_signal: w.handoff_signal.clone(),
        })
        .collect();
    {
        let claims = lock_claims();
        for c in claims.iter().filter(|c| {
            c.workspace_id == req.workspace_id
                && c.thread_slug == thread_slug
                && c.handoff_signal != handoff_signal
        }) {
            live_producers.push(crate::roles::LiveProducer {
                role_slug: c.role_slug.clone(),
                handoff_signal: c.handoff_signal.clone(),
            });
        }
    }

    let role_consumes: Vec<swarmx_protocol::rest::ConsumeRef> = manifest
        .consumes
        .iter()
        .map(|c| swarmx_protocol::rest::ConsumeRef {
            from_role: c.from_role.clone(),
            kind: c.kind.clone(),
        })
        .collect();
    let effective_consumes = if req.consumes.is_empty() {
        role_consumes
    } else {
        req.consumes.clone()
    };
    let depends_on = resolve_consumes_to_deps(
        &registry,
        &role_slug,
        &effective_consumes,
        &req.workspace_id,
        &thread_slug,
        &live_producers,
    )
    .map_err(|msg| (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))))?;

    // W0-4: cycle guard keyed by unique handoff node, not role slug — two
    // parallel researchers must not clobber each other in the HashMap.
    {
        let mut role_handoff: HashMap<String, String> = HashMap::new();
        let mut role_depends: HashMap<String, Vec<String>> = HashMap::new();
        for w in &sibling_workers {
            if w.role_slug.is_empty() {
                continue;
            }
            let node = handoff_graph_node(&w.role_slug, &w.handoff_signal);
            if !w.handoff_signal.is_empty() {
                role_handoff.insert(node.clone(), w.handoff_signal.clone());
            }
            let deps: Vec<String> = serde_json::from_str(&w.depends_on_json).unwrap_or_default();
            role_depends.entry(node).or_default().extend(deps);
        }
        for p in &live_producers {
            let node = handoff_graph_node(&p.role_slug, &p.handoff_signal);
            role_handoff
                .entry(node.clone())
                .or_insert_with(|| p.handoff_signal.clone());
        }
        let pending = handoff_graph_node(&role_slug, &handoff_signal);
        role_handoff.insert(pending.clone(), handoff_signal.clone());
        role_depends
            .entry(pending)
            .or_default()
            .extend(depends_on.iter().cloned());

        if let Err(cycle) = crate::wake::detect_depends_on_cycles(&role_handoff, &role_depends) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!(
                    "spawn rejected: would create a dependency cycle — {cycle}. \
                     Restructure so roles in this direction don't consume each other in a loop."
                ) })),
            ));
        }
    }

    // The orchestrator's prompt + an INPUTS wait-gate (if it has deps) + an
    // explicit copy-verbatim handoff block.
    // B1: substitute {workspace_id}/{thread_slug} in the orchestrator-authored
    // prompt BEFORE appending the handoff plumbing. The orchestrator role doc
    // shows the progress-breadcrumb path as a verbatim `{workspace_id}/{thread_slug}/…`
    // template in backticks; an LLM that copies it literally would send the
    // worker's progress to a literal-placeholder-named blackboard key the Ledger
    // view can never read (looks like the worker silently died). A worker prompt
    // has no {task}/{<role>_id}, so a targeted two-token replace beats render_prompt.
    let rendered_system_prompt = req
        .system_prompt
        .replace("{workspace_id}", &req.workspace_id)
        .replace("{thread_slug}", &thread_slug);
    let system_prompt = build_worker_prompt(
        &rendered_system_prompt,
        &minted_produces,
        &error_key,
        &depends_on,
        &done_checks,
        Some(&progress_key),
    );
    let produces_json = serde_json::to_string(&produces).unwrap_or_else(|_| "[]".to_string());
    let consumes_json =
        serde_json::to_string(&effective_consumes).unwrap_or_else(|_| "[]".to_string());

    // Fork-bomb guard (F4), recursion arm: bound the delegation depth so a
    // worker that spawns a worker that spawns a worker… can't recurse without
    // limit. Walk the parent chain in the workers table from the caller up;
    // the orchestrator is a spell agent (not a worker), so it isn't in the
    // table and the walk terminates there. The global live-agent cap bounds
    // total blast radius; this gives a faster, clearer rejection for deep
    // chains and is loop-bounded so a corrupt/cyclic link can't hang the walk.
    {
        let cap = max_spawn_depth();
        let mut depth = 1usize; // the worker we're about to spawn
        let mut cur = req.caller_agent_id.clone();
        for _ in 0..cap + 2 {
            let rows = state
                .store
                .list_workers_by_ids(vec![cur.clone()])
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("worker depth lookup failed: {e}")})),
                    )
                })?;
            match rows.get(&cur) {
                Some(w) if !w.parent_agent_id.is_empty() => {
                    depth += 1;
                    cur = w.parent_agent_id.clone();
                }
                _ => break,
            }
        }
        if depth > cap {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": format!(
                    "spawn-depth cap reached (depth {depth} > {cap}); refusing to delegate deeper. \
                     Flatten the work or raise SWARMX_MAX_SPAWN_DEPTH."
                )})),
            ));
        }
    }

    // Readiness-gate baseline: any dep written at/after this counts as a
    // CURRENT-run input; earlier writes are stale (prior run) and ignored.
    let worker_spawn_ms = now_ms();
    let resolved_reasoning = thread_reasoning.clone().filter(|s| !s.trim().is_empty());
    let out = spawn_with_bookkeeping(
        &state,
        &resolved_cli,
        Some(role_label.clone()),
        resolved_model.clone(),
        resolved_reasoning,
        layout,
        req.workspace_id.clone(),
        None,              // ad-hoc workers don't belong to a spell run
        thread_id.clone(), // P3③: inherit the caller's direction (None = main)
    )
    .await
    .map_err(|(status, msg)| (status, Json(json!({"error": msg}))))?;

    // P1-D: the worker's dependencies are enforced by the readiness GATE inside
    // spawn_bootstrap_inject (it won't inject the first prompt until every dep —
    // or its `.error` alias — is on the blackboard). So we deliberately do NOT
    // register the worker in wake_subs for its OWN deps: that old re-wake path
    // would fire a PTY kick at the still-un-prompted worker the instant a dep
    // landed, racing the gate's inject and risking a spurious empty turn. The
    // gate polls the blackboard itself and delivers the single first turn.
    // (register_exit_key + the orchestrator's append_wake_sub below stay — they
    // fire when THIS worker writes its handoff, unrelated to its inputs.)
    crate::wake::register_exit_key(
        &state.exit_keys,
        out.agent_id.clone(),
        role_slug.clone(),
        handoff_signal.clone(),
        now_ms(),
        // W2-1: validated above; empty (the default) = no verify gate.
        done_checks.clone(),
    )
    .await;

    // Magentic-One closes the loop here: the orchestrator (the spawning agent)
    // is woken when this worker writes its minted handoff key, so it can read
    // the artifact, update the Progress Ledger, and decide what's next.
    // Append-not-overwrite so it can have many workers in flight at once.
    if !handoff_signal.is_empty() && !req.caller_agent_id.is_empty() {
        crate::wake::append_wake_sub(
            &state.wake_subs,
            req.caller_agent_id.clone(),
            handoff_signal.clone(),
        )
        .await;
    }

    // Persist worker metadata. Failure is non-fatal (PTY is already live),
    // but the DAG view will miss the parent edge until next listAgents
    // refresh after a successful retry. We store the AUGMENTED prompt (what
    // was actually injected) for faithful replay.
    let depends_on_json = serde_json::to_string(&depends_on).unwrap_or_else(|_| "[]".to_string());
    if let Err(e) = state
        .store
        .record_worker(NewWorker {
            agent_id: out.agent_id.clone(),
            parent_agent_id: req.caller_agent_id.clone(),
            role_label: role_label.clone(),
            system_prompt: system_prompt.clone(),
            handoff_signal: handoff_signal.clone(),
            depends_on_json,
            spawned_at: now_ms(),
            role_slug: role_slug.clone(),
            produces_json,
            consumes_json,
        })
        .await
    {
        tracing::warn!(?e, agent = %out.agent_id, "record_worker failed");
    }

    // P1: surface the delegation IN the conversation so multi-agent work is
    // legible in the thread (治诊断1「协作流里不可见」) instead of an opaque new
    // member silently appearing in the roster. A persisted `kind=system` card —
    // from="system" makes send_message fall back to the recipient's (the
    // dispatcher's) direction, so it lands in the right thread. Best-effort,
    // like record_worker above; a failure here doesn't fail the spawn.
    let mut dispatch_meta = serde_json::json!({
        "subtype": "dispatch",
        "child_agent": out.agent_id,
        "child_role": role_label,
        "role_slug": role_slug,
    });
    // Billing red line: if this worker's engine silently fell back (e.g. the
    // orchestrator asked for claude but only API-billed reasonix is installed),
    // stamp it on the persisted card — the SpawnWorkerResponse fields alone
    // only reach the orchestrator LLM, which may never relay them to the user.
    if let Some(from) = &out.fallback_from {
        dispatch_meta["fallback_from"] = serde_json::json!(from);
        dispatch_meta["fallback_cli"] = serde_json::json!(out.cli);
        dispatch_meta["billing_surface"] =
            serde_json::json!(billing_surface_token(out.billing_surface));
    }
    if let Err(e) = state
        .swarm
        .send_message(swarmx_swarm::NewMessage {
            from_agent: "system".to_string(),
            to_agent: req.caller_agent_id.clone(),
            kind: "system".to_string(),
            body: format!("派给 {role_label}"),
            sent_at: now_ms(),
            in_reply_to: None,
            meta: Some(dispatch_meta),
        })
        .await
    {
        tracing::warn!(?e, agent = %out.agent_id, "dispatch system card emit failed");
    }

    // Bootstrap inject — shared with run_spell (see spawn_bootstrap_inject).
    // We inject the AUGMENTED prompt (orchestrator's text + the minted handoff
    // block) so the worker is told the exact canonical key to write.
    spawn_bootstrap_inject(
        state.registry.clone(),
        out.lifecycle_rx.resubscribe(),
        out.agent_id.clone(),
        system_prompt.clone(),
        BootstrapCtx {
            source: "worker",
            spell: String::new(),
            role_keys: Vec::new(),
        },
        depends_on.clone(), // P1-D: gate the first turn on these minted keys
        state.swarm.clone(),
        state.plugins.clone(),
        worker_spawn_ms,
        state.server_url.clone(),
    );

    Ok(Json(SpawnWorkerResponse {
        agent_id: out.agent_id,
        cli: out.cli,
        role_label,
        workspace: out.workspace,
        handoff_signal,
        depends_on,
        fallback_from: out.fallback_from,
        billing_surface: Some(billing_surface_token(out.billing_surface)),
    }))
}

/// `GET /api/roles` — the role registry catalog for `swarm_list_roles` and the
/// UI. Returns the global (builtin + repo) registry; per-workspace
/// `.swarmx/roles/` overrides are applied at spawn time, not here.
pub async fn list_roles(State(state): State<AppState>) -> Json<serde_json::Value> {
    let rows: Vec<serde_json::Value> = state
        .roles
        .list()
        .iter()
        .map(|r| {
            let m = &r.manifest;
            json!({
                "id": m.id,
                "name": m.name,
                "when_to_use": m.when_to_use,
                "default_cli": m.default_cli,
                "default_model_tier": m.default_model_tier,
                "produces": if m.produces.is_empty() { vec!["done".to_string()] } else { m.produces.clone() },
                "modality": m.modality,
                "risk": m.risk,
            })
        })
        .collect();
    Json(json!(rows))
}

// ────────────────────────────────────────────────────────────────────────
// Interrupt / resume (M-pause): operator-controlled pause without tearing
// down the PTY. Cancels the in-flight turn via Ctrl-C (\x03) and flips a
// pause flag that gates the WakeCoordinator's auto-wake path. The PTY,
// blackboard subscription, mailbox, and recording all stay live. Resume
// flips the flag back and delivers one manual wake so any backlog of
// blackboard writes from the paused window gets a fresh look.
// ────────────────────────────────────────────────────────────────────────

pub(crate) async fn interrupt_one_inner(
    registry: &crate::registry::Registry,
    swarm: &swarmx_swarm::Swarm,
    agent_id: &str,
) -> Result<(), String> {
    let slot = registry
        .get(agent_id)
        .ok_or_else(|| format!("agent {agent_id} not found"))?;
    let input_tx = {
        let guard = slot.lock();
        // Set paused FIRST so any in-flight BlackboardChanged event the
        // wake coordinator is currently processing for this agent will
        // see paused=true before we even send the Ctrl-C. The Ordering
        // is Relaxed both here and at the load site — we don't need
        // cross-thread sync beyond visibility.
        guard
            .paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
        match guard.pty_input() {
            Some(tx) => tx,
            // No live PTY to Ctrl-C (agent already exited). The pause flag (set
            // above) still gates auto-wake.
            None => return Ok(()),
        }
    };
    // Best-effort Ctrl-C. If the PTY is already dead (shim_exit fired
    // but registry slot hasn't been removed yet) the send returns Err —
    // we keep paused=true anyway so a re-spawn-into-same-slot scenario
    // can't accidentally start auto-waking again.
    // Best-effort Ctrl-C — and only publish the honest stop signal when it
    // actually went through. If the PTY is already dead (shim_exit fired but the
    // registry slot lingers), broadcasting Idle would transiently mask the real
    // Exited state, so skip it there and let the AgentInfo row flags / tailer
    // carry the truth.
    match input_tx.send(bytes::Bytes::from_static(b"\x03")).await {
        Ok(()) => {
            // P1: an honest stop signal. Without this, interrupting an agent
            // left every surface showing a stale "responding" indicator until
            // the 60s message-inference timeout — the agent's own tailer emits
            // no further state once a turn is cancelled. Idle is the truthful
            // resting state after a cancelled turn (resume re-wakes it). The
            // clicking client also clears optimistically; this covers the member
            // rail, workers, and other clients.
            swarm.publish_event(SwarmEvent::AgentState {
                agent_id: agent_id.to_string(),
                state: AgentState::Idle,
            });
        }
        Err(e) => {
            tracing::warn!(?e, agent = %agent_id, "interrupt Ctrl-C send failed (PTY may be dead); paused flag still set");
        }
    }
    Ok(())
}

/// `POST /api/agent/:id/interrupt` — cancel current turn + pause auto-wake.
pub async fn interrupt(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    match interrupt_one_inner(&state.registry, &state.swarm, &agent_id).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({"ok": true, "agent_id": agent_id, "paused": true})),
        ),
        Err(msg) => (StatusCode::NOT_FOUND, Json(json!({"error": msg}))),
    }
}

/// `POST /api/agent/:id/resume` — clear paused flag + deliver one manual
/// wake to consume any backlog the agent missed while paused. We always
/// deliver the wake (even if the mailbox was empty) so the agent's next
/// Stop hook fire reliably triggers wake-check; this is symmetric with the
/// ⚡ button behavior in `wake_agent`.
pub async fn resume(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    let slot = match state.registry.get(&agent_id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("agent {agent_id} not found")})),
            );
        }
    };
    // Budget brake: resuming while the workspace is still over cap would
    // immediately re-burn. The budget lift path (budget::lift_workspace)
    // resumes brake-paused agents itself after clearing the marker, so it
    // never passes through this gate.
    if let Some(msg) = crate::budget::exceeded_error_for_agent(&state.store, &agent_id).await {
        return (StatusCode::PAYMENT_REQUIRED, Json(json!({"error": msg})));
    }
    slot.lock()
        .paused
        .store(false, std::sync::atomic::Ordering::Relaxed);
    if let Err(e) = crate::wake::deliver_manual_wake(
        &state.swarm,
        &state.registry,
        &state.server_url,
        &agent_id,
    )
    .await
    {
        tracing::warn!(?e, agent = %agent_id, "resume: manual wake failed (paused already cleared)");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string(), "paused": false})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "agent_id": agent_id, "paused": false})),
    )
}

/// `POST /api/agent/interrupt-all?workspace_id=<id>` — interrupt every live
/// agent in a workspace. Live = present in the in-memory registry; killed
/// agents (SQLite-only) are ignored. `workspace_id` is matched against the
/// SQLite-stored value, which means agents spawned via legacy paths
/// without a workspace_id (pre-Step-3) won't be affected — they'd need
/// to be interrupted individually. If `workspace_id` is omitted, errors
/// (we never want to mass-interrupt the entire process).
pub async fn interrupt_all(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let workspace_id = match q
        .get("workspace_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(w) => w.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing required query param 'workspace_id'"})),
            );
        }
    };

    // Resolve which live agents belong to this workspace. The registry
    // slot doesn't carry workspace_id yet (Step 3 of the workspace
    // rollout); we cross-reference SQLite to find matching agent ids.
    let rows = match state.store.list_agents().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?e, "interrupt_all: list_agents store call failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            );
        }
    };
    let target_ids: Vec<String> = rows
        .into_iter()
        .filter(|row| row.killed_at.is_none())
        .filter(|row| row.workspace_id.as_deref() == Some(workspace_id.as_str()))
        .map(|row| row.id)
        .collect();

    let mut interrupted: Vec<String> = Vec::new();
    let mut failed: Vec<serde_json::Value> = Vec::new();
    for id in target_ids {
        match interrupt_one_inner(&state.registry, &state.swarm, &id).await {
            Ok(_) => interrupted.push(id),
            Err(msg) => {
                // Agent may have exited between list_agents and now —
                // skip and report, don't abort the whole batch.
                failed.push(json!({"agent_id": id, "error": msg}));
            }
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "interrupted": interrupted.len(),
            "agent_ids": interrupted,
            "failed": failed,
        })),
    )
}

// ────────────────────────────────────────────────────────────────────────
// Spell endpoints
// ────────────────────────────────────────────────────────────────────────

pub async fn list_spells(State(state): State<AppState>) -> impl IntoResponse {
    // Each [[agents]] entry might be inline (role+cli given) or use
    // role_ref to defer to the RoleRegistry. We resolve here so the UI
    // dropdown shows the actually-effective role/cli rather than the
    // raw "role_ref=frontend / cli=None" the manifest carries.
    // Unresolvable refs (typo, missing role file) are returned as
    // "<role_ref>:?" so the user notices in the dropdown rather than
    // hitting a 500 at run time.
    let items: Vec<SpellInfo> = state
        .spells
        .list()
        .into_iter()
        .map(|s| SpellInfo {
            name: s.manifest.name.clone(),
            description: s.manifest.description.clone(),
            agents: s
                .manifest
                .agents
                .iter()
                .map(|a| match spells::resolve_agent(a, &state.roles) {
                    Ok(r) => SpellAgentInfo {
                        role: r.role,
                        cli: r.cli,
                    },
                    Err(_) => SpellAgentInfo {
                        role: a.effective_role().unwrap_or("?").to_string(),
                        cli: a.cli.clone().unwrap_or_else(|| "?".to_string()),
                    },
                })
                .collect(),
        })
        .collect();
    Json(items)
}

/// Run a spell: spawn all declared agents, wait for each to become ready,
/// then PTY-inject its rendered system_prompt to bootstrap the turn.
///
/// We deliberately fail-soft on per-agent bootstrap injection failures —
/// the spawn already succeeded by that point, and the user can still
/// interact with the agent manually. Returning 500 would mislead them
/// into thinking nothing was spawned, when in fact N agents are now live
/// in the registry.
/// The cli-plugin (engine) the workspace's MOST RECENT orchestrator used —
/// alive or dead — so a respawn (restart auto-respawn, direction rename, cron
/// revive) re-creates the captain on the SAME engine the user picked instead of
/// silently resetting to the role default. `None` (→ role default) if the
/// workspace never had an orchestrator. Used as `RunSpellRequest.captain_cli`.
pub(crate) async fn last_orchestrator_cli(state: &AppState, workspace_id: &str) -> Option<String> {
    let agents = state.store.list_agents().await.ok()?;
    agents
        .into_iter()
        .filter(|a| a.workspace_id.as_deref() == Some(workspace_id) && a.role == "orchestrator")
        .max_by_key(|a| a.spawned_at)
        .map(|a| a.cli)
}

pub async fn run_spell(
    State(state): State<AppState>,
    Json(req): Json<RunSpellRequest>,
) -> Result<Json<RunSpellResponse>, (StatusCode, Json<serde_json::Value>)> {
    let spell = state.spells.get(&req.name).cloned().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown spell: {}", req.name)})),
        )
    })?;

    // Resolve every `[[agents]]` entry against the role registry up-
    // front. Failing here is much friendlier than half-spawning agents
    // and then erroring out — partial spawns are visible PTYs the user
    // would have to kill manually.
    let mut resolved_agents: Vec<spells::ResolvedAgent> = spell
        .manifest
        .agents
        .iter()
        .map(|a| spells::resolve_agent(a, &state.roles))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("spell `{}` resolve failed: {err:#}", req.name)
                })),
            )
        })?;

    // Captain-engine selection for the spell's orchestrator agent(s). Priority:
    //   1. Explicit `RunSpellRequest.captain_cli` — the create wizard passes the
    //      user's pick; cron / auto-respawn / direction-rename pass the workspace's
    //      established engine via `last_orchestrator_cli`.
    //   2. Fallback when omitted: the engine the workspace's most-recent
    //      orchestrator ran on. This covers the bare-`init` callers that DON'T pass
    //      it — notably the manual "唤醒队长 / revive orchestrator" button — which
    //      used to silently reset a non-default captain (reasonix/codex/opencode)
    //      back to the role default (claude) on every revive of a 0-member room.
    //   3. Neither (a brand-new workspace's first init) → the role's `default_cli`.
    // Validated against the plugin registry; an unknown id is ignored (warn)
    // rather than spawning a bogus binary. Worker agents are left untouched.
    let captain_cli = match req.captain_cli.as_deref().filter(|c| !c.is_empty()) {
        Some(cli) => Some(cli.to_string()),
        None => match req.workspace_id.as_deref() {
            Some(ws) => last_orchestrator_cli(&state, ws).await,
            None => None,
        },
    };
    if let Some(cli) = captain_cli.as_deref() {
        if state.plugins.get(cli).is_some() {
            for a in resolved_agents
                .iter_mut()
                .filter(|a| a.role == "orchestrator")
            {
                if a.cli != cli {
                    tracing::info!(spell = %req.name, from = %a.cli, to = %cli, "captain_cli resolved");
                }
                a.cli = cli.to_string();
            }
        } else {
            tracing::warn!(spell = %req.name, cli = %cli, "captain_cli names no known plugin; ignoring");
        }
    }

    // M6b: detect depends_on cycles before any spawn happens. We build
    // the cycle-detection inputs from the resolved agents themselves
    // (role → depends_on) joined with each role's `handoff_signal` (role
    // → key it produces). Roles without a handoff_signal are treated as
    // terminal — they can't be cycled back to.
    {
        let mut role_handoff: HashMap<String, String> = HashMap::new();
        let mut role_deps: HashMap<String, Vec<String>> = HashMap::new();
        for resolved in &resolved_agents {
            role_deps.insert(resolved.role.clone(), resolved.depends_on.clone());
            // Producer key comes off the RESOLVED agent, which carries the
            // referenced role's `handoff_signal` (set in `resolve_agent` via
            // `role_ref`). Re-deriving it here via `state.roles.get(&resolved.role)`
            // — keyed on the SYMBOLIC role name — was the cycle-detection blind
            // spot: a spell that renames a role (`role = "fe"`, `role_ref =
            // "frontend"`) makes that name-lookup MISS, so the producer's key never
            // entered the graph and a role↔role loop through it went undetected.
            // Empty (a truly inline agent with no registered role) → no producer
            // edge, which the detector already treats as terminal.
            if !resolved.handoff_signal.is_empty() {
                role_handoff.insert(resolved.role.clone(), resolved.handoff_signal.clone());
            }
        }
        // M6d-3: a spell can opt out of cycle detection if its prompts
        // explicitly bound the loop (e.g. critic↔fixer in
        // fullstack-feature-strict, capped at 3 rounds by the fixer's
        // round counter). Default behaviour stays: reject cycles.
        let skip_cycle_check = spell.manifest.allow_cycles;
        let cycle_result = if skip_cycle_check {
            tracing::info!(
                spell = %req.name,
                "skipping depends_on cycle check; spell declared allow_cycles=true"
            );
            Ok(())
        } else {
            crate::wake::detect_depends_on_cycles(&role_handoff, &role_deps)
        };
        if let Err(err) = cycle_result {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("spell `{}` has depends_on cycle: {err:#}", req.name)
                })),
            ));
        }
    }

    // Step 3: resolve the effective workspace_id BEFORE picking the
    // layout. Order of precedence:
    //   1. `caller_agent_id` — set when MCP `swarm_run_spell` fires from
    //      inside an existing agent (planner / scout). Reverse-resolves
    //      to the caller's workspace_id so spell agents inherit it.
    //   2. `workspace_id` — set when the UI's launcher / CreateWizard
    //      sends a spell directly.
    //   3. else → 400. This is the core fix for the "orphan workspace
    //      tab" bug: a spell launch with no workspace context is now an
    //      error instead of silently creating an unowned spawn.
    let workspace_id: String = if let Some(caller) = req.caller_agent_id.as_ref() {
        match state.store.get_workspace_id_for_agent(caller.clone()).await {
            Ok(Some(ws_id)) => ws_id,
            Ok(None) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!(
                            "caller agent `{caller}` has no workspace_id; pass workspace_id explicitly"
                        )
                    })),
                ));
            }
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                ));
            }
        }
    } else if let Some(ws_id) = req.workspace_id.clone() {
        ws_id
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "spell requires workspace context: pass workspace_id or caller_agent_id"
            })),
        ));
    };
    // Look up the workspace row so we can default Shared layout cwd to
    // its `cwd` when the client didn't override.
    let workspace = state
        .store
        .get_workspace_by_id(workspace_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("workspace {workspace_id} not found")})),
            )
        })?;

    // Resolve the direction (thread) this spell runs in. Precedence:
    //   1. explicit `req.thread_id` — a UI launcher targeting a direction;
    //   2. the caller's own thread — a sub-spell inherits the orchestrator's;
    //   3. the workspace's main thread — oldest row, auto-created at creation;
    //   4. None — a legacy workspace with no thread rows (= main, cwd = ws.cwd).
    // The resolved thread drives (a) the cwd for isolated directions, (b) the
    // `thread_id` stamped on every spawned agent, and (c) the `{thread_slug}`
    // blackboard prefix so two directions don't clobber each other's ledgers.
    let resolved_thread: Option<ThreadRecord> = {
        let explicit = req.thread_id.clone();
        let caller_tid = if explicit.is_some() {
            None
        } else if let Some(caller) = req.caller_agent_id.as_ref() {
            state
                .store
                .get_thread_id_for_agent(caller.clone())
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        match explicit.or(caller_tid) {
            // `get_thread` returns deleted rows too ("caller checks"), and
            // `req.thread_id` is an untrusted wire field — reject a soft-deleted
            // or foreign-workspace thread. On rejection `resolved_thread` is
            // None, which the rest of the handler treats as the main direction.
            Some(tid) => state
                .store
                .get_thread(tid)
                .await
                .ok()
                .flatten()
                .filter(|t| t.deleted_at.is_none() && t.workspace_id == workspace.id),
            None => state
                .store
                .list_threads(workspace.id.clone())
                .await
                .ok()
                .and_then(|mut v| (!v.is_empty()).then(|| v.remove(0))),
        }
    };
    let thread_id: Option<String> = resolved_thread.as_ref().map(|t| t.id.clone());
    let thread_slug: String = resolved_thread
        .as_ref()
        .map(|t| t.slug.clone())
        .unwrap_or_else(|| "main".to_string());

    // Captain singleton (zero-burden): init / any orch-only spell reuses the
    // live captain instead of spawning a twin that sits silent and burns
    // quota. Model-switch paths kill first, then hit this with an empty room.
    let orch_only =
        !resolved_agents.is_empty() && resolved_agents.iter().all(|a| a.role == "orchestrator");
    if orch_only {
        if let Some(existing) =
            live_orchestrator_in_direction(&state, &workspace.id, thread_id.as_deref()).await
        {
            tracing::info!(
                spell = %req.name,
                agent = %existing.id,
                workspace = %workspace.id,
                "run_spell: reusing live orchestrator (singleton)"
            );
            let billing = state
                .plugins
                .get(&existing.cli)
                .map(|p| billing_surface_token(p.billing_surface));
            return Ok(Json(RunSpellResponse {
                spell: req.name,
                agents: vec![RunSpellAgent {
                    role: existing.role,
                    cli: existing.cli,
                    agent_id: existing.id,
                    fallback_from: None,
                    billing_surface: billing,
                }],
            }));
        }
    }

    // Pick the workspace layout. For shared_workspace spells we use the
    // explicit `workspace_dir` if the client sent one (M6a UX: the
    // SpellsLauncher exposes a text input); otherwise default to the
    // workspace's `cwd` so the spell runs in the project the user picked
    // in CreateWizard. PerAgent spells get per-agent subdirs under
    // `workspaces_root` as before — cwd and workspace_id are orthogonal
    // (filesystem layer vs. UI grouping layer).
    let layout: WorkspaceLayout = if spell.manifest.shared_workspace {
        let dir = match resolved_thread.as_ref() {
            // An isolated direction runs in its own git worktree, full stop —
            // that copy is the whole reason the direction exists.
            Some(t) if t.isolation == "worktree" => PathBuf::from(&t.cwd),
            // Shared / main direction: preserve the M6a `workspace_dir` override
            // (SpellsLauncher text input), else the workspace's own cwd.
            _ => req
                .workspace_dir
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&workspace.cwd)),
        };
        WorkspaceLayout::Shared { dir }
    } else {
        WorkspaceLayout::PerAgent {
            root: state.workspaces_root.clone(),
        }
    };

    // Record the spell-run lineage row so future UI can group agents by
    // "this is the third critic-loop run in critic-demo". Lifetime is
    // tied to the workspace via FK; soft-deleting the workspace doesn't
    // touch this row.
    let spell_run = state
        .store
        .create_spell_run(NewSpellRun {
            workspace_id: workspace.id.clone(),
            spell_name: spell.manifest.name.clone(),
            task: req.task.clone(),
            caller_agent_id: req.caller_agent_id.clone(),
            started_at: now_ms(),
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    let spell_run_id = spell_run.id.clone();

    // Phase 1: spawn all agents up-front (no PTY input yet) so each one's
    // agent_id is known before we render any prompt. Otherwise the writer's
    // prompt couldn't reference critic's id.
    // Per-direction model override: the orchestrator (and any spell agent whose
    // role doesn't pin its own tier) inherits this direction's model_tier. A
    // role with an explicit default_model_tier still wins. None = global default.
    let thread_model_tier: Option<String> = resolved_thread
        .as_ref()
        .and_then(|t| t.model_tier.clone())
        .filter(|s| !s.trim().is_empty());
    // Direction reasoning effort: inherited by every spawned agent (roles don't
    // pin effort). None = the model's own default.
    let thread_reasoning: Option<String> = resolved_thread
        .as_ref()
        .and_then(|t| t.reasoning_effort.clone())
        .filter(|s| !s.trim().is_empty());
    let mut outcomes: Vec<(SpawnOutcome, String)> = Vec::with_capacity(resolved_agents.len());
    for resolved in &resolved_agents {
        let agent_model = state
            .roles
            .get(&resolved.role)
            .map(|r| r.manifest.default_model_tier.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| thread_model_tier.clone());
        let out = spawn_with_bookkeeping(
            &state,
            &resolved.cli,
            Some(resolved.role.clone()),
            agent_model,
            thread_reasoning.clone(),
            layout.clone(),
            workspace.id.clone(),
            Some(spell_run_id.clone()),
            thread_id.clone(),
        )
        .await
        .map_err(|(status, msg)| {
            (
                status,
                Json(json!({
                    "error": format!("spell `{}` failed at agent `{}`: {}", req.name, resolved.role, msg)
                })),
            )
        })?;
        // M6b: register the wake subscription IMMEDIATELY after spawn,
        // before Phase 2 kicks off the deferred bootstrap-inject task.
        // This guards against the race where a producer agent (e.g. BE)
        // is unbelievably fast and writes its handoff key before we get
        // around to subscribing this agent's deps.
        // F2: render {workspace_id}/{task} into depends_on the SAME way the
        // prompt is rendered, so a manifest can write a workspace-scoped key
        // (e.g. "{workspace_id}/api.done") and have it match the producer's
        // rendered handoff_signal — the wake match is exact-string, so an
        // un-substituted placeholder would silently never match. role_to_id
        // isn't built yet here (producers may not be spawned), so {<role>_id}
        // in depends_on can't be resolved; the lint below flags any survivor.
        let empty_roles: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let rendered_deps: Vec<String> = resolved
            .depends_on
            .iter()
            .map(|d| spells::render_prompt(d, &req.task, &workspace_id, &thread_slug, &empty_roles))
            .collect();
        for d in &rendered_deps {
            if d.contains('{') && d.contains('}') {
                tracing::warn!(
                    role = %resolved.role, dep = %d,
                    "spell depends_on still has an unresolved {{...}} placeholder after \
                     {{workspace_id}}/{{task}} substitution — a typo, or a {{<role>_id}} ref \
                     (unsupported in depends_on: the producer's agent_id isn't known when deps \
                     are registered). Wake matching is exact-string and will miss this key."
                );
            }
        }
        crate::wake::register_wake_subs(&state.wake_subs, out.agent_id.clone(), rendered_deps)
            .await;
        // M6c step 5: also remember which signal THIS agent is supposed
        // to produce + the moment we registered it. If the agent exits
        // without writing the signal, the wake coordinator turns that
        // exit into a `<signal>.error` so the downstream dependents
        // stop hanging. The spawn time is used to disambiguate "fresh
        // write from this run's agent" vs "stale leftover from a
        // previous run on the same blackboard". Empty signal (inline
        // role, planner) → register_exit_key is a no-op.
        //
        // Use the RESOLVED agent's handoff_signal (same source as the cycle
        // graph above) instead of re-looking-up by `resolved.role`: a renamed
        // `role_ref` agent name-lookup-misses here, which used to register an
        // EMPTY exit-key, so the producer's death never synthesized
        // `<signal>.error` and dependents hung. Off the resolved template it's
        // correct for both renamed and inline cases.
        let handoff_signal = resolved.handoff_signal.clone();
        // Render the producer's signal the same way (F2) so a workspace-scoped
        // handoff_signal lines up with dependents' rendered depends_on.
        let handoff_signal = spells::render_prompt(
            &handoff_signal,
            &req.task,
            &workspace_id,
            &thread_slug,
            &empty_roles,
        );
        crate::wake::register_exit_key(
            &state.exit_keys,
            out.agent_id.clone(),
            resolved.role.clone(),
            handoff_signal,
            now_ms(),
            // Spell path: no verify gate (empty = legacy behaviour). The gate
            // is declared per-role via `done_checks` and wired on the
            // spawn_worker path only; spells ship no roles with checks.
            Vec::new(),
        )
        .await;
        outcomes.push((out, resolved.system_prompt.clone()));
    }

    // Build role → agent_id map for {<role>_id} substitution.
    let role_to_id: HashMap<String, String> = outcomes
        .iter()
        .map(|(o, _)| (o.role.clone(), o.agent_id.clone()))
        .collect();

    let spell_name = spell.manifest.name.clone();
    // Phase 2: for each agent, wait until shim_ready then inject the
    // rendered prompt. Spawn this off into background tasks so the HTTP
    // response returns promptly — the user wants to see the agents pop
    // up in the UI, not wait 5+ seconds for all bootstraps to land.
    for (out, raw_prompt) in outcomes.iter() {
        if raw_prompt.trim().is_empty() {
            continue;
        }
        let prompt = spells::render_prompt(
            raw_prompt,
            &req.task,
            &workspace_id,
            &thread_slug,
            &role_to_id,
        );
        // Bootstrap inject — shared with spawn_worker (see
        // spawn_bootstrap_inject). Spell agents inject the RENDERED prompt
        // ({task}/{workspace_id}/{<role>_id} substituted above) and pass the
        // role keys so a surviving placeholder is flagged in the log.
        spawn_bootstrap_inject(
            state.registry.clone(),
            out.lifecycle_rx.resubscribe(),
            out.agent_id.clone(),
            prompt,
            BootstrapCtx {
                source: "spell",
                spell: spell_name.clone(),
                role_keys: role_to_id.keys().cloned().collect(),
            },
            // Spell agents (today: the init orchestrator) carry no blackboard
            // deps → empty gate = inject immediately, unchanged behaviour. If a
            // future spell defines dep-bearing role agents, thread their
            // resolved keys here to gate them too.
            Vec::new(),
            state.swarm.clone(),
            state.plugins.clone(),
            now_ms(),
            state.server_url.clone(),
        );
    }

    let resp = RunSpellResponse {
        spell: req.name,
        agents: outcomes
            .into_iter()
            .map(|(o, _)| RunSpellAgent {
                role: o.role,
                cli: o.cli,
                agent_id: o.agent_id,
                // Engine fallback visibility (billing red line): the UI toasts
                // on these — e.g. captain_cli=claude requested but only
                // API-billed reasonix installed must not go unannounced.
                fallback_from: o.fallback_from,
                billing_surface: Some(billing_surface_token(o.billing_surface)),
            })
            .collect(),
    };
    Ok(Json(resp))
}

/// Conservative prompt-improver meta-prompt for the composer 「优化」 button.
/// Tuned for a developer talking to a coding orchestrator: CLARIFY intent
/// WITHOUT HALLUCINATING new requirements. Preserves paths/identifiers/
/// versions/commands verbatim, keeps ambiguity ambiguous, returns near-
/// unchanged input when already clear. (Research basis: OpenAI/Anthropic
/// prompt-improver meta-prompts + Nielsen "prompt augmentation" — the
/// image-tool "pad with detail" model is deliberately rejected here.)
const OPTIMIZE_META_PROMPT: &str = r#"你是一个"提示词编辑器"。下面 <用户原始输入> 里是一名开发者写给 AI 编码调度器(orchestrator)的需求。把它改写成更清晰、更可执行的指令，并严格遵守：

- 保留所有技术细节原样：文件路径、函数/类/变量名、库名与工具名、版本号、命令、报错信息、命令行参数、代码片段。绝不修改、"纠正"或删除它们。
- 不要臆造任何用户没说的需求、约束、验收标准、技术选型、文件名或范围。意图含糊处保持含糊，不要靠猜补全。
- 可以做：修正语法/错别字(代码与标识符内除外)、整理为清晰结构(先目标、再约束、再细节)、把隐含动作显式化、把一长串需求拆成有序步骤。
- 保持简洁，不要加客套、角色扮演或空话。
- 保持与原文相同的语言。
- 如果输入已经足够清晰具体，几乎原样返回。
- 只输出改写后的提示词本身，不要任何前言、解释或 markdown 代码围栏。"#;

#[derive(serde::Deserialize)]
pub struct OptimizePromptRequest {
    pub input: String,
}

#[derive(serde::Serialize)]
pub struct OptimizePromptResponse {
    pub optimized: String,
    pub changed: bool,
}

// (Removed `wait_with_capped_output` + `OPTIMIZE_OUTPUT_CAP`: both the optimize
// and blackboard-compact paths now drive a real interactive claude over PTY via
// `crate::pty_query` for interactive-subscription billing, so there is no longer
// a `claude -p` child whose pipes need bounded draining.)

/// POST /api/prompt/optimize — one-shot prompt rewrite for the chat composer's
/// 「优化」 button.
///
/// Billing: this used to shell out to `claude -p` (print mode), which bills to
/// the Agent-SDK credit pool even under an OAuth subscription (claude-code
/// #43333/#37686; codified 2026-06-15) — so it had to sit behind an opt-in gate.
/// It now drives a REAL interactive claude over a PTY ([`crate::pty_query`]),
/// exactly like every other swarmx agent, so the rewrite bills to the user's
/// *interactive* subscription. No opt-in needed; the gate is gone.
///
/// SAFETY: this rewrites a DRAFT the user has not sent yet, so claude must never
/// *act* on it. The throwaway claude spawns into a temp workspace (nothing of the
/// user's project to touch) and is killed after the single turn; the meta-prompt
/// asks only for a text rewrite, no tool use. A bounded timeout caps the worst
/// case to a clean "timeout" → fall back to the original draft.
pub async fn optimize_prompt(
    State(state): State<AppState>,
    Json(req): Json<OptimizePromptRequest>,
) -> impl IntoResponse {
    let original = req.input.clone();
    let input = req.input.trim().to_string();
    // Empty / too short to meaningfully improve — return unchanged (the button
    // is also disabled client-side; this is the server backstop).
    if input.chars().count() < 8 {
        return Json(OptimizePromptResponse {
            optimized: original,
            changed: false,
        })
        .into_response();
    }

    let plugin = match state.plugins.get("claude") {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "优化需要 claude CLI，但未找到 claude 插件" })),
            )
                .into_response();
        }
    };

    // Small/fast tier so the rewrite stays snappy and cheap, decoupled from the
    // (heavier) orchestrator model.
    let model = { state.models.read().await.resolve(&plugin.id, Some("haiku")) };

    let task = format!("{OPTIMIZE_META_PROMPT}\n\n<用户原始输入>\n{input}\n</用户原始输入>");

    // Drive a throwaway interactive claude over PTY (subscription billing). A
    // cold TUI start + one short turn fits comfortably in 60s.
    // Bound concurrency (F4 sibling): this spawn bypasses the max_live_agents
    // cap, so gate it on the one-shot budget and reject rather than queue.
    let _permit = match state.oneshot_limiter.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "优化服务繁忙（并发上限），请稍后重试" })),
            )
                .into_response();
        }
    };
    let outcome = crate::pty_query::claude_one_shot(
        &plugin,
        &state.shim_path,
        &state.mcp_bin,
        &state.server_url,
        model,
        &task,
        std::time::Duration::from_secs(60),
    )
    .await;

    let text = match outcome {
        Ok(t) => t,
        // Recoverable: no usable result → hand back the original draft unchanged
        // rather than an error toast (the button is best-effort).
        Err(crate::pty_query::OneShotError::Timeout) => {
            return Json(OptimizePromptResponse {
                optimized: original,
                changed: false,
            })
            .into_response();
        }
        // Actionable: surface login / startup failures honestly.
        Err(crate::pty_query::OneShotError::Auth) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(
                    json!({ "error": "优化失败：claude 未登录或未授权，请先在终端 `claude` 登录" }),
                ),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("优化失败：{e}") })),
            )
                .into_response();
        }
    };

    let cleaned = strip_code_fences(text.trim());
    if cleaned.is_empty() {
        // Never hand back an empty composer — fall back to the original.
        return Json(OptimizePromptResponse {
            optimized: original,
            changed: false,
        })
        .into_response();
    }
    let changed = cleaned != original.trim();
    Json(OptimizePromptResponse {
        optimized: cleaned.to_string(),
        changed,
    })
    .into_response()
}

const COMPACT_META_PROMPT: &str = "你是状态摘要器。把下面这份不断累积的「台账/黑板」内容压缩成简洁但**不丢关键信息**的摘要:保留所有未完成事项、关键决策、产出物路径、阻塞与错误;合并重复/过期条目;用简短 Markdown 条目。只输出压缩后的正文,不要任何解释或代码围栏。";

#[derive(serde::Deserialize)]
pub struct CompactBlackboardRequest {
    pub path: String,
}

/// POST /api/blackboard/compact — summarize a long blackboard ledger in place
/// via headless `claude -p` (small tier), to keep accumulated orchestrator
/// state lean. Disabled by default because print/SDK mode can use a separate
/// Claude billing surface; set `SWARMX_ALLOW_CLAUDE_PRINT=1` to opt in. The
/// swarmx-shaped take on "context compression": the PTY CLIs
/// manage their OWN context window, but the blackboard ledgers they read/write
/// grow unbounded — this compacts those. Non-destructive in spirit: the
/// blackboard op-log retains the pre-compaction version. Operator/agent-invoked.
pub async fn compact_blackboard(
    State(state): State<AppState>,
    Json(req): Json<CompactBlackboardRequest>,
) -> impl IntoResponse {
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "path required" })),
        )
            .into_response();
    }
    let content = match state.swarm.read_blackboard(&path).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no such blackboard path" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let before_tokens = crate::tokens::estimate(&content);
    let before_chars = content.chars().count();
    if before_chars < 400 {
        return Json(json!({
            "ok": true, "changed": false,
            "before_tokens": before_tokens, "after_tokens": before_tokens,
            "note": "too small to compact"
        }))
        .into_response();
    }

    let plugin = match state.plugins.get("claude") {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "压缩需要 claude CLI，但未找到 claude 插件" })),
            )
                .into_response();
        }
    };
    let model = { state.models.read().await.resolve(&plugin.id, Some("haiku")) };
    let task = format!("{COMPACT_META_PROMPT}\n\n<台账>\n{content}\n</台账>");

    // Same migration as optimize_prompt: drive a real interactive claude over PTY
    // (subscription billing) instead of `claude -p`, so no opt-in gate is needed.
    // A ledger summary is bigger than a composer rewrite, hence a longer budget.
    // Bound concurrency (F4 sibling): bypasses max_live_agents, so gate it.
    let _permit = match state.oneshot_limiter.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "压缩服务繁忙（并发上限），请稍后重试" })),
            )
                .into_response();
        }
    };
    let outcome = crate::pty_query::claude_one_shot(
        &plugin,
        &state.shim_path,
        &state.mcp_bin,
        &state.server_url,
        model,
        &task,
        std::time::Duration::from_secs(120),
    )
    .await;
    let summary = match outcome {
        Ok(t) => strip_code_fences(t.trim()).to_string(),
        Err(crate::pty_query::OneShotError::Timeout) => {
            // No usable summary → leave the ledger untouched (non-destructive).
            return Json(json!({
                "ok": true, "changed": false,
                "before_tokens": before_tokens, "after_tokens": before_tokens,
                "note": "no smaller summary produced"
            }))
            .into_response();
        }
        Err(crate::pty_query::OneShotError::Auth) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(
                    json!({ "error": "压缩失败：claude 未登录或未授权，请先在终端 `claude` 登录" }),
                ),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("压缩失败：{e}") })),
            )
                .into_response();
        }
    };
    if summary.is_empty() || summary.chars().count() >= before_chars {
        // Don't replace with an empty / bigger result.
        return Json(json!({
            "ok": true, "changed": false,
            "before_tokens": before_tokens, "after_tokens": before_tokens,
            "note": "no smaller summary produced"
        }))
        .into_response();
    }
    if let Err(e) = state
        .swarm
        .write_blackboard(Some("compact".to_string()), &path, &summary)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    Json(json!({
        "ok": true, "changed": true,
        "before_tokens": before_tokens, "after_tokens": crate::tokens::estimate(&summary),
        "before_chars": before_chars, "after_chars": summary.chars().count()
    }))
    .into_response()
}

/// Strip a single ``` fence (with optional language tag) the model may wrap the
/// rewrite in despite being told not to. Leaves un-fenced text untouched.
fn strip_code_fences(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        if let Some(nl) = rest.find('\n') {
            let body = &rest[nl + 1..];
            return body.strip_suffix("```").unwrap_or(body).trim();
        }
    }
    t
}

// ── Local image preview (composer + chat bubbles) ─────────────────────────
// Chat messages reference screenshots by absolute PATH (the same string the
// agents read). These two endpoints let the web UI *show* that image.

/// Map an image extension → MIME, or `None` for non-image extensions (the
/// allowlist — only these are ever served / accepted).
fn image_content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => return None,
    })
}

/// Magic-byte sniff so a renamed non-image (`id_rsa` → `secret.png`) or a
/// prompt-injected agent path to a non-image is rejected even if the extension
/// passes. SVG is text/XML, so it's matched structurally.
fn sniff_is_image(ext: &str, b: &[u8]) -> bool {
    if ext == "svg" {
        let head = &b[..b.len().min(512)];
        let s = String::from_utf8_lossy(head);
        let s = s.trim_start();
        return s.starts_with("<?xml") || s.starts_with("<svg") || s.contains("<svg");
    }
    let starts = |sig: &[u8]| b.len() >= sig.len() && &b[..sig.len()] == sig;
    match ext {
        "png" => starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "jpg" | "jpeg" => starts(&[0xFF, 0xD8, 0xFF]),
        "gif" => starts(b"GIF87a") || starts(b"GIF89a"),
        "webp" => b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP",
        "bmp" => starts(b"BM"),
        "avif" => b.len() >= 12 && &b[4..8] == b"ftyp",
        "ico" => starts(&[0x00, 0x00, 0x01, 0x00]),
        _ => false,
    }
}

/// Loopback-Host guard (DNS-rebinding): a malicious page can't point a rebound
/// domain at us and read file bytes. Absent Host (non-browser caller) is allowed.
fn host_is_loopback(headers: &axum::http::HeaderMap) -> bool {
    match headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
    {
        None => true,
        Some(host) => {
            let h = host.rsplit_once(':').map(|(a, _)| a).unwrap_or(host);
            let h = h.trim_start_matches('[').trim_end_matches(']');
            h == "127.0.0.1" || h.eq_ignore_ascii_case("localhost") || h == "::1"
        }
    }
}

#[derive(serde::Deserialize)]
pub struct FileQuery {
    pub path: String,
}

/// GET /api/file?path=<abs> — serve a LOCAL image so the UI can preview a path
/// referenced in a chat message / composer.
///
/// SECURITY (loopback single-user, but still a real surface): Host must be
/// loopback; a hard credentials/keys denylist (`files::is_sensitive`) is enforced
/// on the canonical path so SSH keys / `.env` / token files are never served even
/// if they sniff as an image; `canonicalize` resolves `..`/symlinks and 404s on
/// missing; an extension allowlist + magic-byte sniff means ONLY real image bytes
/// ever ship (a renamed secret or a prompt-injected non-image path is rejected,
/// so no *text* secret can leak through this endpoint at all); 25 MB cap;
/// `nosniff`; SVG gets a locked-down CSP. We deliberately do NOT add the
/// browser-`Origin` (`is_ui_request`) gate that `files::read_file` uses — this is
/// loaded via a no-cors `<img src>` that sends no `Origin`, so the gate would
/// break every thumbnail once bundled. Nor do we confine to workspace roots:
/// screenshots live anywhere (~/Desktop, /tmp) and images aren't secrets, so
/// "serve real, non-sensitive images from anywhere" is the right balance here.
pub async fn serve_file(
    headers: axum::http::HeaderMap,
    Query(q): Query<FileQuery>,
) -> axum::response::Response {
    use axum::http::header;
    if !host_is_loopback(&headers) {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    let canon = match std::fs::canonicalize(&q.path) {
        Ok(c) => c,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    // Hard credentials/keys/histories denylist on the CANONICAL path — the same
    // backstop `routes::files` enforces on every read, reused here so a local
    // process can't coax `/api/file` into serving a sensitive file even on the
    // off chance its bytes happen to sniff as an image (e.g. an image dropped at
    // a `.pem` name, or an entry inside `~/.ssh`). NOTE: unlike `files::read_file`
    // we deliberately do NOT add an `is_ui_request` (browser-`Origin`) gate here:
    // this endpoint is loaded via a plain `<img src>` (ImageAttachments.tsx), and
    // a no-cors image GET carries NO `Origin` header — gating on it would 403
    // every real thumbnail in the bundled app (the classic "works in dev, broken
    // once installed" trap). The surviving controls — loopback host, magic-byte
    // image sniff (so no text secret like an `.env`/SSH key ever ships, since it
    // won't sniff as an image), this denylist, the 25 MB cap, and `nosniff` —
    // hold the exposure to "a non-sensitive image file anywhere on disk", which
    // is the documented balance (screenshots live in ~/Desktop, /tmp, etc.).
    if super::files::is_sensitive(&canon) {
        return (StatusCode::FORBIDDEN, "not found").into_response();
    }
    match std::fs::metadata(&canon) {
        Ok(m) if m.is_file() => {
            if m.len() > 25 * 1024 * 1024 {
                return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
            }
        }
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    }
    let ext = canon
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let ctype = match image_content_type(&ext) {
        Some(c) => c,
        None => return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not an image").into_response(),
    };
    let bytes = match tokio::fs::read(&canon).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "read failed").into_response(),
    };
    if !sniff_is_image(&ext, &bytes) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not an image").into_response();
    }
    let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, ctype.parse().unwrap());
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert(
        header::CACHE_CONTROL,
        "private, max-age=60".parse().unwrap(),
    );
    if ext == "svg" {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; img-src data:"
                .parse()
                .unwrap(),
        );
    }
    resp
}

#[derive(serde::Deserialize)]
pub struct AttachQuery {
    #[serde(default)]
    pub name: Option<String>,
}

/// POST /api/attachment?name=<basename> — body is raw image bytes (the composer
/// POSTs a pasted/dropped clipboard bitmap here). Saves it under the data dir's
/// `attachments/` and returns its absolute `path`, which the composer drops into
/// the message text so agents can read the image by path (Claude inline, Codex
/// `-i`). Same image-only sniff + size cap as `serve_file`.
pub async fn upload_attachment(
    State(state): State<AppState>,
    Query(q): Query<AttachQuery>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty body").into_response();
    }
    if body.len() > 25 * 1024 * 1024 {
        return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
    }
    // Extension: trust the supplied name only if it's an image AND the bytes
    // actually match; otherwise sniff a supported type from the bytes.
    let name_ext = q
        .name
        .as_deref()
        .and_then(|n| std::path::Path::new(n).extension().and_then(|e| e.to_str()))
        .map(|s| s.to_ascii_lowercase());
    let ext = match name_ext {
        Some(e) if image_content_type(&e).is_some() && sniff_is_image(&e, &body) => e,
        _ => match ["png", "jpeg", "gif", "webp", "bmp", "avif"]
            .into_iter()
            .find(|e| sniff_is_image(e, &body))
        {
            Some(e) => e.to_string(),
            None => return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not an image").into_response(),
        },
    };
    // attachments dir = <data>/attachments (sibling of recordings/blackboard).
    let dir = state
        .recordings_root
        .parent()
        .map(|p| p.join("attachments"))
        .unwrap_or_else(|| state.recordings_root.join("attachments"));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mkdir attachments: {e}"),
        )
            .into_response();
    }
    let full = dir.join(format!("{}.{}", Uuid::new_v4(), ext));
    if let Err(e) = tokio::fs::write(&full, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write attachment: {e}"),
        )
            .into_response();
    }
    Json(json!({ "path": full.to_string_lossy() })).into_response()
}

#[cfg(test)]
mod p0_tests {
    use super::*;
    use crate::roles::RoleRegistry;
    use swarmx_protocol::rest::ConsumeRef;

    fn consume(from: &str, kind: &str) -> ConsumeRef {
        ConsumeRef {
            from_role: from.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn image_sniff_rejects_renamed_non_image() {
        // real PNG magic passes
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert!(sniff_is_image("png", &png));
        // a text file renamed to .png is rejected (the exfil guard)
        assert!(!sniff_is_image(
            "png",
            b"-----BEGIN OPENSSH PRIVATE KEY-----"
        ));
        // jpeg / gif magic
        assert!(sniff_is_image("jpg", &[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(sniff_is_image("gif", b"GIF89a...."));
        // svg detected structurally
        assert!(sniff_is_image("svg", b"<svg xmlns=\"...\"></svg>"));
        assert!(sniff_is_image("svg", b"<?xml version=\"1.0\"?><svg></svg>"));
        assert!(!sniff_is_image("svg", b"just text, not svg"));
        // non-image extension has no content type
        assert!(image_content_type("txt").is_none());
        assert!(image_content_type("png").is_some());
    }

    #[test]
    fn strip_code_fences_unwraps_fenced_output() {
        // bare ``` with language tag
        assert_eq!(
            strip_code_fences("```text\n改写后的内容\n```"),
            "改写后的内容"
        );
        // ``` without language tag
        assert_eq!(strip_code_fences("```\nhello\n```"), "hello");
        // un-fenced text is returned untouched (trimmed)
        assert_eq!(strip_code_fences("  just text  "), "just text");
        // an inline ``` that is not a wrapping fence stays put
        assert_eq!(strip_code_fences("see `x` here"), "see `x` here");
    }

    #[test]
    fn resolve_consumes_happy_mints_scoped_keys() {
        let reg = RoleRegistry::builtin();
        let deps = resolve_consumes_to_deps(
            &reg,
            "reviewer",
            &[consume("backend", "done"), consume("frontend", "done")],
            "ws1",
            "dark-mode",
            &[],
        )
        .unwrap();
        assert_eq!(
            deps,
            vec![
                "ws1/dark-mode/backend.done".to_string(),
                "ws1/dark-mode/frontend.done".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_consumes_binds_all_live_same_role_producers() {
        let reg = RoleRegistry::builtin();
        let live = vec![
            crate::roles::LiveProducer {
                role_slug: "researcher".into(),
                handoff_signal: "ws1/t/researcher.done".into(),
            },
            crate::roles::LiveProducer {
                role_slug: "researcher".into(),
                handoff_signal: "ws1/t/researcher.aabbccdd.done".into(),
            },
        ];
        let deps = resolve_consumes_to_deps(
            &reg,
            "orchestrator",
            &[consume("researcher", "done")],
            "ws1",
            "t",
            &live,
        )
        .unwrap();
        assert_eq!(
            deps,
            vec![
                "ws1/t/researcher.done".to_string(),
                "ws1/t/researcher.aabbccdd.done".to_string(),
            ]
        );
        assert!(crate::roles::pick_handoff_instance(live.len()).is_some());
    }

    #[test]
    fn handoff_graph_node_keeps_parallel_researchers_distinct() {
        let a = handoff_graph_node("researcher", "ws/t/researcher.done");
        let b = handoff_graph_node("researcher", "ws/t/researcher.aabbccdd.done");
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_consumes_unknown_role_suggests_closest() {
        let reg = RoleRegistry::builtin();
        let err = resolve_consumes_to_deps(
            &reg,
            "reviewer",
            &[consume("bakend", "done")],
            "ws1",
            "main",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("unknown role 'bakend'"), "got: {err}");
        assert!(err.contains("did you mean 'backend'"), "got: {err}");
    }

    #[test]
    fn resolve_consumes_rejects_kind_not_produced() {
        let reg = RoleRegistry::builtin();
        // builtin backend produces ["done"] only.
        let err = resolve_consumes_to_deps(
            &reg,
            "reviewer",
            &[consume("backend", "spec")],
            "ws1",
            "main",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("does not produce kind 'spec'"), "got: {err}");
    }

    #[test]
    fn resolve_consumes_rejects_self_dependency() {
        let reg = RoleRegistry::builtin();
        let err = resolve_consumes_to_deps(
            &reg,
            "frontend",
            &[consume("frontend", "done")],
            "ws1",
            "main",
            &[],
        )
        .unwrap_err();
        assert!(err.contains("self-dependency"), "got: {err}");
    }

    #[test]
    fn resolve_consumes_rejects_empty_from_role() {
        let reg = RoleRegistry::builtin();
        let err =
            resolve_consumes_to_deps(&reg, "frontend", &[consume("", "done")], "ws1", "main", &[])
                .unwrap_err();
        assert!(err.contains("empty from_role"), "got: {err}");
    }

    #[test]
    fn closest_match_within_threshold_only() {
        let cands = vec![
            "frontend".to_string(),
            "backend".to_string(),
            "reviewer".to_string(),
        ];
        assert_eq!(
            closest_match("fronend", &cands).as_deref(),
            Some("frontend")
        );
        // Garbage far from everything → no suggestion.
        assert_eq!(closest_match("zzzzzzzz", &cands), None);
    }

    #[test]
    fn build_worker_prompt_injects_minted_keys_and_error_branch() {
        let p = build_worker_prompt(
            "do the thing",
            &["ws1/main/frontend.done".to_string()],
            "ws1/main/frontend.done.error",
            &[],
            &[],
            None,
        );
        assert!(p.starts_with("do the thing"));
        assert!(p.contains("ws1/main/frontend.done"));
        assert!(p.contains("ws1/main/frontend.done.error"));
        assert!(p.contains("VERBATIM"));
        assert!(!p.contains("INPUTS"), "no deps → no inputs gate");
        assert!(
            !p.contains("VERIFY GATE"),
            "no done_checks → no verify gate"
        );
        let with_progress = build_worker_prompt(
            "do the thing",
            &["ws1/t/researcher.aabb.done".to_string()],
            "ws1/t/researcher.aabb.done.error",
            &[],
            &[],
            Some("ws1/t/researcher.aabb.progress.md"),
        );
        assert!(with_progress.contains("ws1/t/researcher.aabb.progress.md"));
        assert!(with_progress.contains("PROGRESS BREADCRUMBS"));
        // No keys at all → prompt returned unchanged (fire-and-forget, no deps).
        assert_eq!(
            build_worker_prompt("x", &[], "x.error", &[], &[], None),
            "x"
        );
    }

    #[test]
    fn build_worker_prompt_verify_gate_block_only_when_checks_declared() {
        // W2-1: declared done_checks add a VERIFY GATE block naming the exact
        // commands, so the worker self-checks before declaring done.
        let p = build_worker_prompt(
            "do the thing",
            &["ws1/main/backend.done".to_string()],
            "ws1/main/backend.done.error",
            &[],
            &["cargo test".to_string(), "cargo clippy".to_string()],
            None,
        );
        assert!(p.contains("VERIFY GATE"));
        assert!(p.contains("- cargo test"));
        assert!(p.contains("- cargo clippy"));
        assert!(p.contains("bounced back"), "names the failure semantics");
        // Still carries the handoff block above it.
        assert!(p.contains("ws1/main/backend.done"));
    }

    #[test]
    fn readiness_gate_first_unsatisfied_dep() {
        use crate::agent_lifecycle::first_unsatisfied_dep;
        use std::collections::HashSet;
        let deps = vec![
            "ws/main/backend.done".to_string(),
            "ws/main/db.ready".to_string(),
        ];

        // nothing present → first dep is unsatisfied
        let empty = HashSet::new();
        assert_eq!(
            first_unsatisfied_dep(&deps, &empty).as_deref(),
            Some("ws/main/backend.done")
        );

        // first present, second missing → returns the second
        let mut p1: HashSet<String> = HashSet::new();
        p1.insert("ws/main/backend.done".into());
        assert_eq!(
            first_unsatisfied_dep(&deps, &p1).as_deref(),
            Some("ws/main/db.ready")
        );

        // both present → all satisfied
        let mut p2 = p1.clone();
        p2.insert("ws/main/db.ready".into());
        assert_eq!(first_unsatisfied_dep(&deps, &p2), None);

        // a `.error` alias counts as satisfied (fail-loud: wake to handle it)
        let mut p3: HashSet<String> = HashSet::new();
        p3.insert("ws/main/backend.done.error".into());
        p3.insert("ws/main/db.ready.failed".into());
        assert_eq!(first_unsatisfied_dep(&deps, &p3), None);

        // empty deps → never blocks
        assert_eq!(first_unsatisfied_dep(&[], &empty), None);
    }

    #[test]
    fn gate_watchdog_shield_only_after_long_wait() {
        use crate::agent_lifecycle::gate_should_shield_watchdog;
        use std::time::Duration;
        // Short gate waits must NOT stamp activity: `last_activity_at` is
        // permanent once set, and a worker that clears the gate quickly and
        // then truly wedges before any sign of life has to stay catchable by
        // the one-shot watchdog.
        assert!(!gate_should_shield_watchdog(Duration::ZERO));
        assert!(!gate_should_shield_watchdog(Duration::from_secs(59)));
        // At/past the shield point the 90s watchdog (the minimum window) fires
        // DURING the wait → shield the legitimate silence.
        assert!(gate_should_shield_watchdog(Duration::from_secs(60)));
        assert!(gate_should_shield_watchdog(Duration::from_secs(299)));
    }

    #[tokio::test]
    async fn gated_worker_shield_touch_defuses_watchdog_probe() {
        // Regression: a readiness-gated worker waits silently past the
        // first-response watchdog window, and the watchdog's one-shot probe
        // (`agent_silent_since_ready`) must NOT flag it once the gate's shield
        // touch has landed. Exercised at the store layer — the gate loop's
        // timings are wall-clock minutes, too slow for a unit test; the probe's
        // message/usage/killed signals are covered by swarmx-storage's own
        // `agent_silent_since_ready_reflects_signs_of_life`.
        let dir = tempfile::tempdir().unwrap();
        let store = swarmx_storage::Store::open(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let at = now_ms();
        store
            .record_agent_spawn(NewAgent {
                id: "w-gated".into(),
                cli: "claude".into(),
                role: "backend".into(),
                workspace: dir.path().join("ws").to_string_lossy().into_owned(),
                spawned_at: at,
                workspace_id: None,
                spell_run_id: None,
                thread_id: None,
            })
            .await
            .unwrap();
        store.record_shim_ready("w-gated".into(), at).await.unwrap();
        // Bug precondition: ready but silent (still gated) → the probe flags
        // it — exactly what the watchdog mistook for a wedge.
        assert!(store
            .agent_silent_since_ready("w-gated".into())
            .await
            .unwrap());
        // What the gate now does on every poll while holding past the shield
        // point → the probe goes false and the watchdog stands down.
        store
            .touch_agent_activity("w-gated".into(), now_ms())
            .await
            .unwrap();
        assert!(!store
            .agent_silent_since_ready("w-gated".into())
            .await
            .unwrap());
    }

    #[test]
    fn build_worker_prompt_adds_inputs_wait_gate_when_deps_present() {
        let p = build_worker_prompt(
            "review it",
            &["ws1/main/reviewer.done".to_string()],
            "ws1/main/reviewer.done.error",
            &["ws1/main/backend.done".to_string()],
            &[],
            None,
        );
        // The wait-gate must name the dep and forbid acting / writing handoff early.
        assert!(p.contains("INPUTS"));
        assert!(p.contains("ws1/main/backend.done"));
        assert!(p.contains("do NOT write your"));
        assert!(p.contains("STOP"));
        // Still carries its own handoff block.
        assert!(p.contains("ws1/main/reviewer.done"));
        // A dep-only worker with no produces still gets the inputs gate.
        let q = build_worker_prompt(
            "x",
            &[],
            "x.error",
            &["ws1/main/dep.done".to_string()],
            &[],
            None,
        );
        assert!(q.contains("INPUTS"));
        assert!(q.contains("ws1/main/dep.done"));
    }

    #[test]
    fn install_hint_for_codex_prefers_official_installer() {
        let plugin: CliPlugin =
            toml::from_str("id='codex'\ndisplay_name='Codex CLI'\nbinary='codex'\n").unwrap();
        let hint = install_hint_for(&plugin).expect("codex hint");
        assert_eq!(hint.docs_url, "https://github.com/openai/codex");
        assert_eq!(
            hint.commands.first().map(String::as_str),
            Some("curl -fsSL https://chatgpt.com/codex/install.sh | sh")
        );
        assert!(hint.commands.iter().any(|c| c.contains("@openai/codex")));
        assert_eq!(hint.login_command.as_deref(), Some("codex login"));
    }

    #[test]
    fn missing_cli_install_message_lists_recovery_steps() {
        let plugin: CliPlugin =
            toml::from_str("id='claude'\ndisplay_name='Claude Code'\nbinary='claude'\n").unwrap();
        let msg = missing_cli_install_message(&plugin);
        assert!(msg.contains("Claude Code CLI binary `claude`"));
        assert!(msg.contains("curl -fsSL https://claude.ai/install.sh | bash"));
        assert!(msg.contains("https://code.claude.com/docs/en/setup"));
    }

    fn test_registry() -> PluginRegistry {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("claude.toml"),
            "id='claude'\ndisplay_name='Claude Code'\nbinary='claude'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("codex.toml"),
            "id='codex'\ndisplay_name='Codex CLI'\nbinary='codex'\n",
        )
        .unwrap();
        PluginRegistry::load_dir(dir.path()).unwrap()
    }

    #[test]
    fn select_spawn_plugin_keeps_requested_when_available() {
        let registry = test_registry();
        let (selected, fallback_from) =
            select_spawn_plugin_with(&registry, "claude", &|p| p.id == "claude").unwrap();
        assert_eq!(selected.id, "claude");
        assert!(fallback_from.is_none());
    }

    #[test]
    fn select_spawn_plugin_unknown_cli_suggests_and_lists() {
        // A typo'd engine must get the same self-correctable hint the unknown-
        // ROLE path gives: a did-you-mean + the valid list, not a dead-end.
        let registry = test_registry();
        let err = select_spawn_plugin_with(&registry, "claud", &|_| true).unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(
            err.1.contains("unknown cli plugin: claud"),
            "msg: {}",
            err.1
        );
        assert!(
            err.1.contains("did you mean 'claude'"),
            "missing suggestion: {}",
            err.1
        );
        assert!(
            err.1.contains("valid engines:"),
            "missing valid list: {}",
            err.1
        );
    }

    #[test]
    fn select_spawn_plugin_falls_back_to_installed_codex() {
        let registry = test_registry();
        let (selected, fallback_from) =
            select_spawn_plugin_with(&registry, "claude", &|p| p.id == "codex").unwrap();
        assert_eq!(selected.id, "codex");
        assert_eq!(fallback_from.map(|p| p.id.as_str()), Some("claude"));
    }

    #[test]
    fn select_spawn_plugin_reports_install_hints_when_none_available() {
        let registry = test_registry();
        let err = select_spawn_plugin_with(&registry, "claude", &|_| false).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("Claude Code CLI binary `claude`"));
        assert!(err
            .1
            .contains("curl -fsSL https://claude.ai/install.sh | bash"));
        assert!(err.1.contains("Other supported AI engines"));
        assert!(err
            .1
            .contains("Codex CLI: curl -fsSL https://chatgpt.com/codex/install.sh | sh"));
    }

    #[test]
    fn billing_surface_token_matches_serde_kebab_rename() {
        // The token feeds UI copy ("按 API 计费") on an engine fallback, so pin
        // it to the manifest's serde spelling — reasonix is the ApiKey-billed
        // fallback this surfacing exists for (its requires_explicit_billing_opt_in
        // is false, making it a legal silent fallback candidate).
        assert_eq!(
            billing_surface_token(crate::plugins::BillingSurface::ApiKey),
            "api-key"
        );
        assert_eq!(
            billing_surface_token(crate::plugins::BillingSurface::InteractiveSubscription),
            "interactive-subscription"
        );
        assert_eq!(
            billing_surface_token(crate::plugins::BillingSurface::Unknown),
            "unknown"
        );
    }
}

#[cfg(test)]
mod bootstrap_needle_tests {
    use crate::agent_lifecycle::wait_for_pty_needle;
    use bytes::Bytes;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn needle_found_in_existing_buffer() {
        let stream = Arc::new(crate::pty_stream::PtyStream::new());
        stream.append(Bytes::from_static(b"boot noise\r\n"));
        stream.append(Bytes::from_static(
            b"MCP server \"swarmx-swarm\" connected \xc2\xb7 11 tools (stdio)",
        ));
        assert!(
            wait_for_pty_needle(
                &stream,
                b"MCP server \"swarmx-swarm\" connected",
                Duration::from_millis(200),
            )
            .await
        );
    }

    #[tokio::test]
    async fn needle_stitched_across_chunks() {
        let stream = Arc::new(crate::pty_stream::PtyStream::new());
        stream.append(Bytes::from_static(b"MCP server \"swarmx-swa"));
        let s2 = stream.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            s2.append(Bytes::from_static(b"rm\" connected \xc2\xb7 11 tools"));
        });
        assert!(
            wait_for_pty_needle(
                &stream,
                b"MCP server \"swarmx-swarm\" connected",
                Duration::from_secs(2),
            )
            .await
        );
    }

    #[tokio::test]
    async fn needle_absent_times_out() {
        let stream = Arc::new(crate::pty_stream::PtyStream::new());
        stream.append(Bytes::from_static(b"some other output"));
        assert!(
            !wait_for_pty_needle(
                &stream,
                b"never-appearing-banner",
                Duration::from_millis(150),
            )
            .await
        );
    }

    #[tokio::test]
    async fn needle_absent_stream_closed_is_false() {
        let stream = Arc::new(crate::pty_stream::PtyStream::new());
        stream.append(Bytes::from_static(b"early exit"));
        stream.close();
        assert!(
            !wait_for_pty_needle(&stream, b"never-appearing-banner", Duration::from_secs(5),).await,
            "a closed stream must not wait out the full timeout"
        );
    }
}
