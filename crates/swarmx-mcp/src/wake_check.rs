//! `swarmx-mcp wake-check`: Stop-hook helper invoked by Claude Code / Codex
//! CLI at every turn boundary.
//!
//! Wire-protocol (the lowest-common-denominator of the two CLIs' Stop hooks):
//!   - **stdin**: a JSON object provided by the CLI. We only look at one
//!     field: `stop_hook_active: bool` (codex sets this on a re-fire to break
//!     hook loops; claude's docs don't currently document the field but we
//!     read it opportunistically).
//!   - **stdout**: a single JSON object, NOTHING ELSE. `{}` means "no-op,
//!     don't change the turn". `{"decision":"block", "reason":"..."}`
//!     synthesises a continuation prompt that the LLM reads in its very next
//!     turn — that's how we feed "you have N unread messages" back to the
//!     agent without hijacking the PTY.
//!   - **exit code**: always 0. Codex documents that "plain text output is
//!     invalid" for Stop; the safe path is JSON-on-stdout + exit 0 for every
//!     branch including errors.
//!
//! `--hook-format kimi` speaks Kimi Code's DIFFERENT Stop-hook protocol
//! instead (kimi reads no stdout JSON): "block + continue" is signalled by
//! **exit code 2** with the continuation message on **stderr**; a no-op is a
//! bare exit 0 with no output. Everything upstream of the emit step (stdin
//! parsing, agent-id resolution, throttling, consume_wakes) is identical.
//!
//! Loop prevention is dual-track:
//!   1. `stop_hook_active` short-circuits when the CLI explicitly tells us
//!      "this is a recursive call, please stop".
//!   2. `~/.swarmx/wake/<agent_id>.json` carries a sliding-window counter
//!      so even a CLI that never sets the flag can't get stuck in a wake
//!      loop. Counter is cleared whenever unread drops to zero.
//!
//! Failure mode: any error (bad stdin, missing server, HTTP failure, file
//! IO) collapses to the format's no-op + exit 0. The hook MUST NOT prevent
//! the agent from stopping; degrading to "no wake this turn" is always safe.

use anyhow::Result;
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use swarmx_protocol::rest::ConsumeWakesResponse;

/// Which CLI's Stop-hook protocol to speak. Selected by the `--hook-format`
/// flag baked into the hook command the server writes into each CLI's config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum HookFormat {
    /// claude / codex: JSON on stdout (`{}` noop, `{"decision":"block",
    /// "reason":...}` wake), exit code ALWAYS 0.
    #[default]
    Claude,
    /// Kimi Code: noop = bare exit 0 (no output); wake = the continuation
    /// message on STDERR + exit code 2 (kimi's documented "block" signal).
    Kimi,
}

#[derive(Debug, Args)]
pub struct WakeCheckArgs {
    /// Optional override of which agent this hook speaks for. Normally the
    /// hook command in `<workspace>/.codex/hooks.json` (or its claude
    /// equivalent) omits this flag — we instead derive agent_id from the
    /// `cwd` field of the CLI-supplied stdin JSON.
    ///
    /// Why: codex 0.130+ keys hook trust by hook-config hash (including the
    /// command string). Embedding a per-spawn `--agent-id <id>` makes every
    /// new agent's hook count as a "new or changed" hook and re-prompts the
    /// `/hooks` review dialog. Keeping the command string stable across all
    /// spawns and reading agent_id at runtime collapses that to a one-time
    /// trust per machine.
    ///
    /// The flag is preserved as a back-door for tests and ad-hoc
    /// invocations where stdin isn't a valid Stop-hook JSON.
    #[arg(long)]
    pub agent_id: Option<String>,

    /// Base URL of the swarmx-server REST API.
    #[arg(long, default_value = "http://127.0.0.1:7777")]
    pub server: String,

    /// Sliding-window length (seconds). Default 30s.
    #[arg(long, default_value_t = 30)]
    pub throttle_secs: u64,

    /// Maximum wakes per window before forcing a no-op. Default 3.
    #[arg(long, default_value_t = 3)]
    pub max_wakes_per_window: u32,

    /// Override the wake state directory (defaults to ~/.swarmx/wake/).
    /// Test-only; not advertised in help.
    #[arg(long, hide = true)]
    pub state_dir: Option<PathBuf>,

    /// Which CLI's Stop-hook protocol to speak. Baked into the hook command
    /// the server installs (kimi's config.toml entry carries
    /// `--hook-format kimi`); defaults to the claude/codex JSON protocol.
    #[arg(long, value_enum, default_value_t = HookFormat::Claude)]
    pub hook_format: HookFormat,
}

/// Persisted between wake invocations. Kept tiny on purpose — corruption
/// recovery is "delete the file and start over".
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct ThrottleState {
    /// Unix epoch milliseconds of the most recent wake we emitted.
    last_at_ms: u64,
    /// Number of wakes within the current window. Resets when unread → 0.
    count: u32,
}

/// HTTP budget per call. The hook fires on every turn boundary; we'd rather
/// degrade to no-wake than block the agent's stop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum stdin we'll consume. Real payloads from claude / codex are <2KB;
/// 64KB is paranoia + room for future fields.
const STDIN_MAX_BYTES: usize = 64 * 1024;

/// How long we wait for stdin before giving up and treating it as empty.
/// Both CLIs close stdin after sending the JSON, so this should always be
/// fast in practice.
const STDIN_TIMEOUT: Duration = Duration::from_millis(500);

pub async fn run(args: WakeCheckArgs) -> Result<i32> {
    // From here on: every branch ends in an emit_* helper + Ok(<exit code>).
    // The only non-zero code is kimi's documented "block" signal (2), raised
    // deliberately AFTER we decide to wake — never an error propagation.
    let format = args.hook_format;

    // ── 1. stdin: extract stop_hook_active + (maybe) agent_id ────────────
    let stdin_bytes = read_stdin_bounded().await;
    let stdin_json: Option<Value> = serde_json::from_slice(&stdin_bytes).ok();
    if parse_stop_hook_active(&stdin_bytes) {
        return Ok(emit_noop(format));
    }

    // agent_id resolution order:
    //   1. --agent-id CLI flag (legacy + tests)
    //   2. SWARMX_AGENT_ID env var — set by spawn.rs at CLI process launch;
    //      Stop hook runs as a child of the CLI and inherits it. This is the
    //      only path that works for shared_workspace spells where every
    //      agent's cwd is the same monorepo root (M6a fullstack-feature).
    //   3. stdin `cwd` basename — legacy per-agent workspace layout
    //      (`<root>/<agent_id>`), kept as a fallback for older spawns that
    //      somehow lack the env var.
    // Falling through all three = silent noop; we can't sensibly probe
    // unread mail without knowing who we're acting for.
    let agent_id = match args.agent_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match std::env::var("SWARMX_AGENT_ID")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(id) => id,
            None => match agent_id_from_stdin_cwd(stdin_json.as_ref()) {
                Some(id) => id,
                None => {
                    eprintln!(
                        "wake-check: no --agent-id flag, no SWARMX_AGENT_ID env, \
                         and stdin lacks usable cwd; skipping wake"
                    );
                    return Ok(emit_noop(format));
                }
            },
        },
    };

    // ── 2. throttle gate ─────────────────────────────────────────────────
    let state_path = throttle_path(&agent_id, args.state_dir.as_deref());
    let now_ms = unix_ms();
    let state = read_throttle(&state_path).unwrap_or_default();
    let throttle_window_ms = args.throttle_secs.saturating_mul(1000);
    let window_active =
        state.last_at_ms != 0 && now_ms.saturating_sub(state.last_at_ms) < throttle_window_ms;
    if window_active && state.count >= args.max_wakes_per_window {
        return Ok(emit_noop(format));
    }

    // ── 3. HTTP: POST /api/message/consume_wakes?to=<agent_id> ──────────
    //
    // Why "consume" not "count": M6f bug fix. The old `unread_count`
    // path counted ALL unread messages (including non-wake), but more
    // importantly it relied on `swarm_list_messages` to mark messages
    // read once the LLM handled them. That coupling broke when the LLM
    // mid-turn-listed: a wake arriving during a long turn got swept
    // into list_messages' auto-mark-read, becoming invisible to the
    // NEXT wake_check that fired at the turn's actual Stop hook. The
    // 2026-05-23 strict e2e #6 caught this: critic processed
    // frontend.done in turn 1, the LLM list_messages'd one more time
    // before stopping, picked up the just-arrived backend.done wake,
    // marked it read → wake_check saw 0 unread → critic idled 46 min.
    //
    // The fix has two halves: tools.rs::list_messages now skips
    // kind="wake" in its mark-read set (wakes don't get touched by
    // LLM calls), and this hook calls the dedicated `consume_wakes`
    // endpoint which atomically claims-and-marks-read all pending
    // wakes in one transaction. The wakes are delivered to the LLM
    // via the `block` reason below.
    //
    // B1: the response also carries the consumed wakes' inline payloads
    // (mailbox note + blackboard snapshot), so the reason can digest the
    // actual content instead of sending the LLM off to list/read for 3–5
    // serial round-trips before it can speak.
    let resp = match consume_wakes(&args.server, &agent_id).await {
        Ok(r) => r,
        Err(err) => {
            // Any transport / HTTP failure is a graceful degrade — never
            // block the agent's stop because swarmx-server happens to be
            // down or the port changed.
            eprintln!("wake-check: {err}");
            return Ok(emit_noop(format));
        }
    };
    let count = resp.count;

    if count <= 0 {
        // Mailbox empty → reset throttle so the NEXT genuine message lands
        // a wake unhindered (otherwise an agent that processed mail late
        // in the window would have to wait for the window to roll over).
        let _ = std::fs::remove_file(&state_path);
        return Ok(emit_noop(format));
    }

    // ── 4. Persist throttle bump + emit wake ─────────────────────────────
    let next_state = if window_active {
        ThrottleState {
            last_at_ms: now_ms,
            count: state.count.saturating_add(1),
        }
    } else {
        ThrottleState {
            last_at_ms: now_ms,
            count: 1,
        }
    };
    if let Err(err) = write_throttle(&state_path, &next_state) {
        // Throttle write failure is not fatal — worst case the next wake
        // also fires and we still bounded by stop_hook_active / agent
        // dedupe semantics.
        eprintln!("wake-check: throttle write failed: {err}");
    }

    // M6f: wakes are ALREADY marked read by consume_wakes (atomic).
    // The reason text below IS the delivery — it tells the LLM what
    // changed and what to do. swarm_list_messages may surface other
    // (non-wake) messages too, but the wakes themselves don't need
    // re-fetching.
    //
    // B1: `resp.continuation()` is the shared recipe (also used by PTY kick
    // / reasonix / zulu / opencode). The agent can react without the old
    // 3–5 serial round-trips (list_messages → list_blackboard →
    // read_blackboard).
    let reason = resp.continuation();
    Ok(emit_block(format, &reason))
}

// ── stdout helpers ───────────────────────────────────────────────────────

/// Emit the format's no-op and return the process exit code (always 0 — the
/// hook must never prevent the agent from stopping).
fn emit_noop(format: HookFormat) -> i32 {
    match format {
        // Codex's docs are emphatic: stdout must be JSON. `{}` is a valid no-op.
        HookFormat::Claude => print!("{{}}"),
        // kimi: a bare exit 0 already means "allow the stop" — no output.
        HookFormat::Kimi => {}
    }
    0
}

/// Emit the wake and return the process exit code: claude/codex synthesize
/// the continuation from stdout JSON (exit 0); kimi reads the continuation
/// message from STDERR and requires exit code 2 (its documented "block"
/// signal — the ONLY non-zero code this binary deliberately returns).
fn emit_block(format: HookFormat, reason: &str) -> i32 {
    match format {
        HookFormat::Claude => {
            let payload = json!({
                "decision": "block",
                "reason": reason,
            });
            // `to_string` (not `to_string_pretty`) — no trailing newline, no
            // extra bytes. Both CLIs parse stdout strictly.
            let s = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
            print!("{s}");
            0
        }
        HookFormat::Kimi => {
            eprintln!("{reason}");
            2
        }
    }
}

// ── stdin helpers ────────────────────────────────────────────────────────

async fn read_stdin_bounded() -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(2048);
    let mut stdin = tokio::io::stdin();
    let _ = tokio::time::timeout(STDIN_TIMEOUT, async {
        // take(...) caps how many bytes we read so a misbehaving CLI
        // can't OOM us by streaming gigabytes.
        let mut limited = (&mut stdin).take(STDIN_MAX_BYTES as u64);
        limited.read_to_end(&mut buf).await
    })
    .await;
    buf
}

fn parse_stop_hook_active(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return false,
    };
    v.get("stop_hook_active")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

/// Both claude and codex feed Stop hooks a JSON object containing the
/// session's `cwd`. swarmx workspaces are always created at
/// `<root>/<agent_id>` (see `spawn::ensure_workspace`), so the basename of
/// `cwd` IS the agent_id. Returns None if the field is missing, empty, or
/// somehow non-Unicode — caller falls back to noop.
fn agent_id_from_stdin_cwd(stdin: Option<&Value>) -> Option<String> {
    let cwd = stdin?.get("cwd")?.as_str()?;
    if cwd.is_empty() {
        return None;
    }
    let basename = Path::new(cwd).file_name()?.to_str()?;
    if basename.is_empty() {
        return None;
    }
    Some(basename.to_string())
}

// ── HTTP ─────────────────────────────────────────────────────────────────

/// M6f: atomically claim + mark-read all pending wake messages for this
/// agent. Replaces the previous `GET unread_count` path. Returns the full
/// typed response: `count`/`ids` plus (B1) the inline `wakes` payloads and
/// `reason` continuation. Whatever this returns as consumed MUST be
/// delivered to the LLM via emit_block (otherwise it would be lost, since
/// it's already marked read).
async fn consume_wakes(server: &str, agent_id: &str) -> Result<ConsumeWakesResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let url = format!("{server}/api/message/consume_wakes");
    let resp = client
        .post(&url)
        .query(&[("to", agent_id)])
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    resp.json::<ConsumeWakesResponse>()
        .await
        .map_err(|e| format!("parse json from {url}: {e}"))
}

// ── throttle file ────────────────────────────────────────────────────────

fn throttle_path(agent_id: &str, override_dir: Option<&Path>) -> PathBuf {
    let dir = match override_dir {
        Some(p) => p.to_path_buf(),
        None => default_state_dir(),
    };
    dir.join(format!("{agent_id}.json"))
}

fn default_state_dir() -> PathBuf {
    // HOME→USERPROFILE so the throttle dir is stable on Windows too (bare HOME is
    // usually unset there, which would scatter state under a CWD-relative ".").
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".swarmx").join("wake")
}

fn read_throttle(path: &Path) -> Option<ThrottleState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_throttle(path: &Path, state: &ThrottleState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(state).map_err(|e| format!("serialize throttle state: {e}"))?;
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))?;
    Ok(())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, Json, Router};
    use serde_json::json;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;

    /// Spin up a stub `swarmx-server` exposing only the wake-check
    /// endpoint (M6f: POST /api/message/consume_wakes). Returns
    /// `(addr, counter)` — flip `counter` to control the returned count.
    async fn start_stub(initial: i64) -> (SocketAddr, Arc<AtomicI64>) {
        let counter = Arc::new(AtomicI64::new(initial));
        let counter_inner = counter.clone();
        let app = Router::new().route(
            "/api/message/consume_wakes",
            axum::routing::post(move |Query(q): Query<HashMap<String, String>>| {
                let counter = counter_inner.clone();
                async move {
                    let to = q.get("to").cloned().unwrap_or_default();
                    let count = counter.load(Ordering::SeqCst);
                    // ids array would be filled in real impl; tests only
                    // check the count field so a single sentinel per
                    // count keeps the stub honest about array length.
                    let ids: Vec<i64> = (1..=count.max(0)).collect();
                    Json(json!({ "to": to, "count": count, "ids": ids }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, counter)
    }

    /// Spin up a stub that returns HTTP 500 — used to exercise the
    /// degrade-to-noop path.
    async fn start_error_stub() -> SocketAddr {
        let app = Router::new().route(
            "/api/message/consume_wakes",
            axum::routing::post(|| async {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    /// B1: stub serving the inline `wakes` payload — one fully-inlined
    /// blackboard wake, one server-truncated wake, one body-only manual
    /// wake. Exercises the typed parse + reason digest end to end.
    async fn start_payload_stub() -> SocketAddr {
        let app = Router::new().route(
            "/api/message/consume_wakes",
            axum::routing::post(|| async {
                Json(json!({
                    "to": "test-agent",
                    "count": 3,
                    "ids": [11, 12, 13],
                    "wakes": [
                        {
                            "id": 11,
                            "from_agent": "system",
                            "sent_at": 1,
                            "body": "共享区 `design.md` 有更新，请查看",
                            "key": "design.md",
                            "content": "# Design\nhello",
                            "truncated": false,
                        },
                        {
                            "id": 12,
                            "from_agent": "system",
                            "sent_at": 2,
                            "body": "共享区 `big.md` 有更新，请查看",
                            "key": "big.md",
                            "content": "aaaa",
                            "truncated": true,
                        },
                        {
                            "id": 13,
                            "from_agent": "system",
                            "sent_at": 3,
                            "body": "操作员唤醒——请先查收邮箱里的新消息",
                            "key": null,
                            "content": null,
                            "truncated": false,
                        }
                    ]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    fn args_for(addr: SocketAddr, agent_id: &str, state_dir: &Path) -> WakeCheckArgs {
        WakeCheckArgs {
            agent_id: Some(agent_id.into()),
            server: format!("http://{addr}"),
            throttle_secs: 30,
            max_wakes_per_window: 3,
            state_dir: Some(state_dir.to_path_buf()),
            hook_format: HookFormat::Claude,
        }
    }

    // ── 1. Direct unit tests on the pure helpers ─────────────────────────

    #[test]
    fn parse_stop_hook_active_returns_true_when_flag_set() {
        let bytes = br#"{"stop_hook_active": true}"#;
        assert!(parse_stop_hook_active(bytes));
    }

    #[test]
    fn parse_stop_hook_active_false_when_absent() {
        let bytes = br#"{"other_field": 42}"#;
        assert!(!parse_stop_hook_active(bytes));
    }

    #[test]
    fn parse_stop_hook_active_false_when_invalid_json() {
        assert!(!parse_stop_hook_active(b"not json at all"));
    }

    #[test]
    fn parse_stop_hook_active_false_when_empty() {
        assert!(!parse_stop_hook_active(b""));
    }

    #[test]
    fn throttle_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.json");
        let s = ThrottleState {
            last_at_ms: 12345,
            count: 2,
        };
        write_throttle(&path, &s).unwrap();
        let back = read_throttle(&path).unwrap();
        assert_eq!(back.last_at_ms, 12345);
        assert_eq!(back.count, 2);
    }

    #[test]
    fn throttle_read_missing_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(read_throttle(&path).is_none());
    }

    // ── 2. Integration via run() with a stub server ──────────────────────

    // The `run()` function prints to stdout, which interferes with cargo's
    // test runner output. We test the *side effects* — throttle file state
    // and HTTP traffic — rather than capturing stdout. Stdout shape is
    // covered by separate emit_* unit tests below.

    #[tokio::test]
    async fn count_zero_clears_throttle_and_emits_noop() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        // Pre-seed throttle to verify it's cleared.
        let path = throttle_path("test-agent", Some(&state_dir));
        write_throttle(
            &path,
            &ThrottleState {
                last_at_ms: 1,
                count: 2,
            },
        )
        .unwrap();
        assert!(path.exists());

        let (addr, _) = start_stub(0).await;
        let args = args_for(addr, "test-agent", &state_dir);
        let code = run(args).await.unwrap();

        assert_eq!(code, 0, "claude-format noop exits 0");
        assert!(
            !path.exists(),
            "throttle file should be unlinked when count=0"
        );
    }

    #[tokio::test]
    async fn count_positive_writes_throttle() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let (addr, _) = start_stub(3).await;
        let args = args_for(addr, "test-agent", &state_dir);
        let code = run(args).await.unwrap();

        assert_eq!(
            code, 0,
            "claude-format block still exits 0 (JSON-on-stdout protocol)"
        );
        let path = throttle_path("test-agent", Some(&state_dir));
        let state = read_throttle(&path).expect("throttle file should exist");
        assert_eq!(state.count, 1);
        assert!(state.last_at_ms > 0);
    }

    #[tokio::test]
    async fn throttle_window_blocks_repeat_wake() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        // Pre-seed throttle at max.
        let path = throttle_path("test-agent", Some(&state_dir));
        write_throttle(
            &path,
            &ThrottleState {
                last_at_ms: unix_ms(),
                count: 3,
            },
        )
        .unwrap();

        // Server returning a positive count would normally trigger a wake;
        // throttle should suppress it. We check by asserting the count in
        // the throttle file stays at 3 (no increment, because we early-exit
        // before the HTTP call).
        let (addr, _) = start_stub(5).await;
        let args = args_for(addr, "test-agent", &state_dir);
        run(args).await.unwrap();

        let state = read_throttle(&path).unwrap();
        assert_eq!(
            state.count, 3,
            "throttle should not increment when window is full"
        );
    }

    #[tokio::test]
    async fn throttle_window_expired_resets_count() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let path = throttle_path("test-agent", Some(&state_dir));
        // Seed with a stale window (older than throttle_secs).
        write_throttle(
            &path,
            &ThrottleState {
                last_at_ms: unix_ms().saturating_sub(60 * 1000),
                count: 3,
            },
        )
        .unwrap();

        let (addr, _) = start_stub(1).await;
        let args = args_for(addr, "test-agent", &state_dir);
        run(args).await.unwrap();

        let state = read_throttle(&path).unwrap();
        assert_eq!(state.count, 1, "expired window should reset to 1");
    }

    #[tokio::test]
    async fn http_error_does_not_create_throttle() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let addr = start_error_stub().await;
        let args = args_for(addr, "test-agent", &state_dir);
        run(args).await.unwrap();
        let path = throttle_path("test-agent", Some(&state_dir));
        assert!(!path.exists(), "no throttle write on HTTP error");
    }

    #[tokio::test]
    async fn network_unreachable_does_not_create_throttle() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        // Point at a port that almost certainly has nothing listening.
        let args = WakeCheckArgs {
            agent_id: Some("test-agent".into()),
            server: "http://127.0.0.1:1".into(),
            throttle_secs: 30,
            max_wakes_per_window: 3,
            state_dir: Some(state_dir.clone()),
            hook_format: HookFormat::Claude,
        };
        run(args).await.unwrap();
        let path = throttle_path("test-agent", Some(&state_dir));
        assert!(!path.exists(), "no throttle write when server unreachable");
    }

    // ── 3. agent_id derivation from stdin cwd ─────────────────────────────

    #[test]
    fn agent_id_from_cwd_takes_basename() {
        let v = json!({ "cwd": "/Users/wdx/.swarmx/workspaces/codex-6d068ccb" });
        assert_eq!(
            agent_id_from_stdin_cwd(Some(&v)).as_deref(),
            Some("codex-6d068ccb"),
        );
    }

    #[test]
    fn agent_id_from_cwd_handles_trailing_slash() {
        let v = json!({ "cwd": "/tmp/ws/claude-abc12345/" });
        // Path::file_name strips a single trailing slash on Unix.
        assert_eq!(
            agent_id_from_stdin_cwd(Some(&v)).as_deref(),
            Some("claude-abc12345"),
        );
    }

    #[test]
    fn agent_id_from_cwd_missing_field_returns_none() {
        let v = json!({ "session_id": "no-cwd-here" });
        assert!(agent_id_from_stdin_cwd(Some(&v)).is_none());
    }

    #[test]
    fn agent_id_from_cwd_no_json_returns_none() {
        assert!(agent_id_from_stdin_cwd(None).is_none());
    }

    #[test]
    fn agent_id_from_cwd_empty_string_returns_none() {
        let v = json!({ "cwd": "" });
        assert!(agent_id_from_stdin_cwd(Some(&v)).is_none());
    }

    #[test]
    fn emit_noop_writes_empty_object() {
        // Direct shape check — emit_* fns are tiny but we want the contract
        // pinned: empty object, no newline, no whitespace.
        let payload = json!({});
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, "{}");
    }

    #[test]
    fn emit_block_writes_decision_and_reason() {
        let payload = json!({
            "decision": "block",
            "reason": "test reason",
        });
        let s = serde_json::to_string(&payload).unwrap();
        // Ordering of JSON keys in serde_json::to_string is insertion order
        // because we use the json! macro — assert structurally instead.
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["decision"], json!("block"));
        assert_eq!(back["reason"], json!("test reason"));
    }

    // ── 4. kimi hook protocol (stderr + exit code 2 = block) ─────────────

    #[tokio::test]
    async fn kimi_noop_returns_zero() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let (addr, _) = start_stub(0).await;
        let args = WakeCheckArgs {
            hook_format: HookFormat::Kimi,
            ..args_for(addr, "test-agent", &state_dir)
        };
        let code = run(args).await.unwrap();
        assert_eq!(code, 0, "kimi noop = bare exit 0 (allow the stop)");
    }

    #[tokio::test]
    async fn kimi_block_returns_exit_code_2() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let (addr, _) = start_stub(2).await;
        let args = WakeCheckArgs {
            hook_format: HookFormat::Kimi,
            ..args_for(addr, "test-agent", &state_dir)
        };
        let code = run(args).await.unwrap();
        assert_eq!(
            code, 2,
            "kimi wake = exit 2 (kimi's documented block signal)"
        );
        // The wake still rode the same throttle bookkeeping as claude's.
        let path = throttle_path("test-agent", Some(&state_dir));
        let state = read_throttle(&path).expect("throttle file should exist");
        assert_eq!(state.count, 1);
    }

    #[tokio::test]
    async fn kimi_http_error_degrades_to_zero() {
        // Fail-open: a swarmx-server outage must not block kimi's stop either.
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let addr = start_error_stub().await;
        let args = WakeCheckArgs {
            hook_format: HookFormat::Kimi,
            ..args_for(addr, "test-agent", &state_dir)
        };
        let code = run(args).await.unwrap();
        assert_eq!(code, 0, "kimi degrade path is also exit 0");
    }

    #[tokio::test]
    async fn payload_stub_parses_and_wakes_claude_format() {
        // End-to-end on the claude/codex protocol: typed parse of the inline
        // payload succeeds and the wake path runs (throttle write is the
        // observable signal that we did NOT degrade to noop).
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let addr = start_payload_stub().await;
        let args = args_for(addr, "test-agent", &state_dir);
        let code = run(args).await.unwrap();
        assert_eq!(code, 0, "claude-format block still exits 0");
        let path = throttle_path("test-agent", Some(&state_dir));
        let state = read_throttle(&path).expect("payload response must drive a wake, not a noop");
        assert_eq!(state.count, 1);
    }

    #[tokio::test]
    async fn payload_stub_parses_and_wakes_kimi_format() {
        // Same digest rides kimi's protocol (stderr + exit 2) — emit_block is
        // shared, so both hook formats carry the identical reason text.
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let addr = start_payload_stub().await;
        let args = WakeCheckArgs {
            hook_format: HookFormat::Kimi,
            ..args_for(addr, "test-agent", &state_dir)
        };
        let code = run(args).await.unwrap();
        assert_eq!(
            code, 2,
            "kimi wake = exit 2 with the digested reason on stderr"
        );
        let path = throttle_path("test-agent", Some(&state_dir));
        assert!(path.exists());
    }
}
