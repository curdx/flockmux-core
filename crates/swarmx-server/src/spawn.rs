//! Wire `swarmx-shim <real-cli> <args...>` together for a single agent.

use crate::plugins::CliPlugin;
use crate::pty_stream::PtyStream;
use crate::registry::{AgentChannel, AgentSlot, Lifecycle, LifecycleEvent};
use anyhow::{Context, Result};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use swarmx_pty::{PtyBridge, PtyHandles, SpawnOpts};
use swarmx_recorder::RecorderHandle;
use tokio::sync::mpsc;
use uuid::Uuid;

// OSC markers the shim emits — kept identical to swarmx-shim/src/main.rs.
const OSC_READY: &[u8] = b"\x1b]633;A\x07";
const OSC_EXIT_PREFIX: &[u8] = b"\x1b]633;D;";
const OSC_TERMINATOR: u8 = 0x07;

pub struct AgentSpawn {
    pub agent_id: String,
    pub slot: AgentSlot,
    /// For claude: the `--session-id` UUID we force at spawn, so the transcript
    /// tailer locates `~/.claude/projects/<enc-cwd>/<uuid>.jsonl` EXACTLY rather
    /// than guessing "newest .jsonl" (a stale prior-session file in the same
    /// project dir would otherwise win). None for codex — it locates via its
    /// isolated per-agent CODEX_HOME, which has no stale-file problem.
    pub transcript_session_id: Option<String>,
}

/// How [`spawn_agent`] resolves the workspace directory for a fresh
/// agent. Two strategies cover every current call site:
///
/// - [`WorkspaceLayout::PerAgent`] — the historical default: each agent
///   gets its own `<root>/<agent_id>/` subdirectory. Used by
///   `POST /api/agent` and by spells that don't set
///   `shared_workspace`.
/// - [`WorkspaceLayout::Shared`] — every caller agent runs in the same
///   absolute directory. M6a fullstack-feature spells use this so FE /
///   BE / Test peer agents see the same monorepo (`apps/frontend`,
///   `apps/backend`, `tests/`).
///
/// Kept as an enum (not a `bool` parameter) so future strategies — e.g.
/// `Worktree(git_repo, branch)` for M6b — slot in without touching
/// every call site again.
#[derive(Debug, Clone)]
pub enum WorkspaceLayout {
    PerAgent { root: PathBuf },
    Shared { dir: PathBuf },
}

/// `shim_path` is the absolute path to `swarmx-shim`. Caller normally
/// derives it from `std::env::current_exe()` parent + "swarmx-shim".
///
/// `mcp_bin` is the absolute path to `swarmx-mcp` (the swarm MCP stdio
/// server). It's baked into per-spawn MCP config entries written under
/// pre-spawn patches so claude / codex can launch it on first tool call.
///
/// `server_url` is the base URL of the swarmx-server REST API that the
/// MCP subprocess will speak to. Loopback today.
///
/// `recorder` is an optional asciicast v2 sink. When set, the PTY pump
/// mirrors every chunk (including OSC lifecycle markers) into the
/// recorder; when unset, the recording layer is bypassed.
/// Environment variables forwarded from the server's env into a spawned worker
/// IF present — non-secret runtime essentials (locale, proxy, TLS CA bundles)
/// plus the load-bearing identity vars. `USER` / `LOGNAME` are required for
/// macOS Keychain access: claude stores its OAuth token in the login keychain
/// and the Security framework resolves it via `$USER`. Dropping `USER` makes a
/// logged-in claude print "Not logged in" — a real regression (it was missing
/// from this allowlist once) that `forwarded_env_keeps_macos_keychain_vars`
/// guards against. `HOME` and `PATH` are inserted unconditionally above, not
/// via this list.
pub(crate) const FORWARDED_ENV_KEYS: &[&str] = &[
    "USER",
    "LOGNAME",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    // Windows: a child needs these to even start — system DLLs live under
    // SystemRoot, plus per-user dirs, temp, and the .cmd launcher (ComSpec /
    // PATHEXT). All absent on unix, so forwarding them there is a no-op.
    "SystemRoot",
    "windir",
    "ComSpec",
    "PATHEXT",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

#[allow(clippy::too_many_arguments)]
pub fn spawn_agent(
    plugin: &CliPlugin,
    role: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    workspace: &WorkspaceLayout,
    shim_path: &Path,
    mcp_bin: &Path,
    server_url: &str,
    recorder: Option<RecorderHandle>,
) -> Result<AgentSpawn> {
    let agent_id = format!("{}-{}", plugin.id, &Uuid::new_v4().to_string()[..8]);
    let workspace = match workspace {
        WorkspaceLayout::PerAgent { root } => ensure_workspace(root, &agent_id)?,
        WorkspaceLayout::Shared { dir } => ensure_shared_workspace(dir)?,
    };

    enforce_billing_policy(plugin)?;

    // Resolve THE adapter for this CLI once. Everything CLI-specific below —
    // pre-spawn patches, argv/env contributions, transcript session id — is
    // delegated to it so this function stays generic machinery. Every CLI runs
    // over the PTY; opencode additionally gets a `--port` for its TUI HTTP
    // control API (see the input_delivery block after the argv contributions).
    let adapter = crate::cli::adapter_for(plugin);
    tracing::debug!(agent = %agent_id, cli = %plugin.id, adapter = adapter.name(), "resolved CLI adapter");

    // Per-CLI pre-spawn host patches: pre-accept trust prompts, dismiss update
    // nags, inject the swarmx-swarm MCP server, install the wake hook. Each
    // capability is gated inside the adapter on the plugin's `auto_*` flags and
    // is a no-op when not configured.
    let pre_ctx = crate::cli::PreSpawnCtx {
        agent_id: agent_id.clone(),
        mcp_bin: mcp_bin.to_path_buf(),
        server_url: server_url.to_string(),
    };
    adapter.pre_spawn(plugin, &workspace, &pre_ctx);

    let mut argv = Vec::with_capacity(2 + plugin.default_args.len() + 1);
    argv.push(shim_path.to_string_lossy().into_owned());
    // Resolve the CLI to an absolute, PATHEXT-aware path before handing it to the
    // shim (which execs it via std Command). On Windows the npm-installed CLIs are
    // `claude.cmd`/`zulu.cmd` etc. and std only searches for `.exe`, so a bare
    // name never launches; resolve_executable finds the `.cmd`. On unix this is
    // the absolute path (or the bare-name fallback) — behaviour unchanged.
    argv.push(
        crate::runtime_path::resolve_executable(&plugin.binary)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| plugin.binary.clone()),
    );
    argv.extend(plugin.default_args.iter().cloned());

    // zulu resolves its model PER-REQUEST (not an argv — see cli-plugins/
    // zulu.toml), so capture the resolved value here for the ZuluConv the
    // serve driver reads. No-op for every other CLI.
    let zulu_model = if plugin.input_delivery == crate::plugins::InputDelivery::ZuluServeHttp {
        model.clone().or_else(|| plugin.default_model.clone())
    } else {
        None
    };

    // L5c model overlay: a model passed at spawn time (REST/MCP) wins over the
    // plugin's default_model. The flag itself (claude & codex both use
    // `--model <v>`) lives in the manifest's `model_args` template, not here —
    // host ≠ model: the same CLI runs any model with zero Rust/role forking.
    match model.or_else(|| plugin.default_model.clone()) {
        Some(m) if !plugin.model_args.is_empty() => {
            argv.extend(model_overlay_args(&m, &plugin.model_args));
            tracing::info!(agent = %agent_id, model = %m, "model overlay applied");
        }
        Some(m) if plugin.input_delivery == crate::plugins::InputDelivery::ZuluServeHttp => {
            // zulu resolves its model per-request (captured into ZuluConv above),
            // so an empty model_args is BY DESIGN — not the misconfiguration the
            // warn below flags. Nothing to add to argv.
            tracing::debug!(agent = %agent_id, model = %m, "zulu model rides on the serve request, not argv");
        }
        Some(m) => tracing::warn!(
            agent = %agent_id, model = %m,
            "model requested but plugin declares no model_args; ignoring"
        ),
        None => {}
    }

    // Reasoning/thinking effort overlay (parallel to model). The abstract level
    // (low|medium|high|max) maps to this CLI's concrete value via the manifest's
    // effort_levels; the result substitutes into effort_args (claude `--effort`,
    // codex `-c model_reasoning_effort=`). Unknown/"default" level or a CLI with
    // no effort support → emit nothing = the model's own default.
    if let Some(level) = reasoning
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match plugin.effort_levels.get(level) {
            Some(concrete) if !plugin.effort_args.is_empty() => {
                argv.extend(effort_overlay_args(concrete, &plugin.effort_args));
                tracing::info!(agent = %agent_id, effort = %level, concrete = %concrete, "reasoning effort overlay applied");
            }
            _ => tracing::debug!(
                agent = %agent_id, effort = %level,
                "reasoning effort requested but unmapped / unsupported by this CLI; ignoring"
            ),
        }
    }

    // Per-CLI argv contributions, applied after the model/effort overlay:
    // codex's `--dangerously-bypass-hook-trust` (probed for binary support),
    // claude's per-agent `--mcp-config … --strict-mcp-config` (dodges the
    // shared-cwd ~/.claude.json collision). Each adapter knows its own flags and
    // formats; this is a no-op for a CLI that needs neither.
    adapter.contribute_argv(plugin, &agent_id, &mut argv);

    // opencode: drive its TUI over the documented `/tui/*` HTTP control API
    // (append-prompt + submit-prompt) instead of keystroke injection — its TUI
    // can't take a large bootstrap via bracketed paste (it parks at READY and
    // never submits). We spawn the TUI on a known ephemeral `--port` and the
    // bootstrap / wake paths POST prompts there. The port is remembered on the
    // slot so those paths can reach it. See `crate::opencode_tui`.
    let mut tui_http_port: Option<u16> = None;
    if plugin.input_delivery == crate::plugins::InputDelivery::OpencodeTuiHttp {
        match alloc_ephemeral_port() {
            Some(port) => {
                argv.push("--port".into());
                argv.push(port.to_string());
                tui_http_port = Some(port);
                tracing::info!(agent = %agent_id, port, "opencode TUI HTTP control port allocated");
            }
            None => tracing::warn!(
                agent = %agent_id,
                "could not allocate a TUI HTTP control port for opencode; \
                 bootstrap/wake delivery will fail"
            ),
        }
    }

    // reasonix is driven over its `reasonix serve` HTTP+SSE control API. Like
    // opencode we allocate a known ephemeral port, but reasonix takes it as
    // `--addr 127.0.0.1:<port>` (not `--port`). The port rides on the slot so the
    // bootstrap path can start the SSE driver and the wake path can POST /submit.
    // See `crate::reasonix_serve`.
    let mut serve_http_port: Option<u16> = None;
    if plugin.input_delivery == crate::plugins::InputDelivery::ReasonixServeHttp {
        match alloc_ephemeral_port() {
            Some(port) => {
                argv.push("--addr".into());
                argv.push(format!("127.0.0.1:{port}"));
                serve_http_port = Some(port);
                tracing::info!(agent = %agent_id, port, "reasonix serve HTTP control port allocated");
            }
            None => tracing::warn!(
                agent = %agent_id,
                "could not allocate a serve HTTP control port for reasonix; \
                 bootstrap/wake delivery will fail"
            ),
        }
    }

    // zulu (Comate) is driven over `zulu serve` HTTP+SSE. Alloc a port passed
    // as `--host 127.0.0.1 --port <port>` (not reasonix's `--addr`). The model
    // is per-request, so it rides on the ZuluConv (with the license + cwd) the
    // bootstrap/wake driver reads, NOT the argv. License via `-l` from
    // COMATE_LICENSE for now (P1.4 wires the settings-page source). See
    // `crate::zulu_serve`.
    let mut zulu_conv: Option<std::sync::Arc<crate::zulu_serve::ZuluConv>> = None;
    if plugin.input_delivery == crate::plugins::InputDelivery::ZuluServeHttp {
        match alloc_ephemeral_port() {
            Some(port) => {
                argv.push("--host".into());
                argv.push("127.0.0.1".into());
                argv.push("--port".into());
                argv.push(port.to_string());
                let license = crate::comate::load_license();
                if !license.is_empty() {
                    argv.push("-l".into());
                    argv.push(license.clone());
                }
                serve_http_port = Some(port);
                zulu_conv = Some(std::sync::Arc::new(crate::zulu_serve::ZuluConv::new(
                    port,
                    zulu_model.clone().unwrap_or_default(),
                    license,
                    workspace.to_string_lossy().into_owned(),
                    server_url.to_string(),
                )));
                tracing::info!(agent = %agent_id, port, "zulu serve HTTP control port allocated");
            }
            None => tracing::warn!(
                agent = %agent_id,
                "could not allocate a serve HTTP control port for zulu; \
                 bootstrap/wake delivery will fail"
            ),
        }
    }

    // (`CommandBuilder::env_clear`), so the worker sees ONLY what we insert
    // here. We forward what a CLI legitimately needs (HOME for OAuth creds,
    // PATH, locale, proxy/TLS, and the provider's own config/keys) but NOT
    // arbitrary shell secrets the server happened to be launched with
    // (AWS_*, GITHUB_TOKEN, DB creds, …). This is the allowlist that realises
    // the per-agent isolation goal.
    let mut env = HashMap::new();
    // Home dir for OAuth creds. On Windows HOME is usually unset (USERPROFILE
    // holds it), so route the default through swarmx_home() rather than a bare
    // env read; a plugin-specified home_env still reads that exact var. Set both
    // HOME and USERPROFILE so a CLI reading either resolves its config.
    let home = if plugin.home_env.is_empty() {
        crate::runtime_path::swarmx_home().map(|p| p.to_string_lossy().into_owned())
    } else {
        std::env::var(&plugin.home_env).ok()
    };
    if let Some(home) = home {
        #[cfg(windows)]
        env.insert("USERPROFILE".into(), home.clone());
        env.insert("HOME".into(), home);
    }
    // Useful unicode default — many CLIs probe LANG.
    if let Ok(lang) = std::env::var("LANG") {
        env.insert("LANG".into(), lang);
    } else {
        env.insert("LANG".into(), "en_US.UTF-8".into());
    }
    // PATH: preserve the parent's path and append desktop install locations.
    // Finder-launched macOS apps get a short launchd PATH, while node/npm/npx
    // and the CLIs commonly live under Homebrew, Volta, asdf, cargo, or
    // ~/.local/bin. The allowlist keeps this deterministic without importing a
    // user's whole shell environment.
    env.insert(
        "PATH".into(),
        crate::runtime_path::augmented_path()
            .to_string_lossy()
            .into_owned(),
    );
    // Non-secret runtime essentials the CLI may need to reach the network /
    // render unicode. Forwarded only if present — see FORWARDED_ENV_KEYS for
    // the full list plus the macOS-Keychain rationale behind USER/LOGNAME.
    for &key in FORWARDED_ENV_KEYS {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.into(), v);
        }
    }
    // Provider config/creds the inner CLI authenticates with, matched by
    // prefix so e.g. ANTHROPIC_BASE_URL / OPENAI_BASE_URL pass through to the
    // CLI that declared them, while unrelated *_TOKEN / *_KEY shell secrets
    // do NOT. `or_insert` so explicit entries below (e.g. per-agent
    // CODEX_HOME) always win over an inherited value.
    //
    // SECURITY: each plugin declares ITS OWN provider prefixes
    // (`provider_env_prefixes` in cli-plugins/<id>.toml) and only those are
    // forwarded. The previous hard-coded ANTHROPIC_/OPENAI_/CLAUDE_/DEEPSEEK_
    // list handed every provider's keys to every plugin — codex children got
    // ANTHROPIC_API_KEY, claude children got DEEPSEEK_API_KEY — and these
    // CLIs are full-shell agents, so an ambient foreign key is plaintext
    // handed to the model. `blocked_env_prefixes` still intersects (claude
    // declares ANTHROPIC_ but blocks it by default — the billing red line).
    for (k, v) in std::env::vars() {
        if forward_provider_env(plugin, &k) {
            env.entry(k).or_insert(v);
        } else if provider_env_declared(plugin, &k) && env_blocked(plugin, &k) {
            // A prefix this plugin DID declare but blocks by policy (claude's
            // ANTHROPIC_ — the billing red line) is worth a debug line;
            // undeclared vars are the normal case and stay silent.
            tracing::debug!(
                agent = %agent_id,
                cli = %plugin.id,
                env = %k,
                "provider env var blocked by plugin policy"
            );
        }
    }
    // Identity env passed to the CLI. codex picks `SWARMX_AGENT_ID` /
    // `SWARMX_SERVER_URL` up via the `env_vars` whitelist in
    // ~/.codex/config.toml and forwards them to the MCP subprocess. claude
    // also forwards them by spec (any vars present in the spawn env that
    // match the MCP entry's `env` block) — and the local-scope MCP entry
    // we write already lists them explicitly, so this is belt + braces.
    env.insert("SWARMX_AGENT_ID".into(), agent_id.clone());
    env.insert("SWARMX_SERVER_URL".into(), server_url.to_string());

    // Per-CLI env contributions beyond the shared allowlist: codex's per-agent
    // CODEX_HOME (isolates MCP from the user's global ~/.codex), opencode's
    // per-agent OPENCODE_CONFIG. No-op for a CLI that needs neither.
    adapter.contribute_env(plugin, &agent_id, &mut env);

    // claude pins a known session id (pushing `--session-id`) so the transcript
    // tailer locates the exact JSONL instead of guessing the newest file in the
    // project dir; codex/opencode return None (codex locates via its per-agent
    // CODEX_HOME).
    let transcript_session_id = adapter.transcript_session_id(plugin, &agent_id, &mut argv);

    let argv_strings: Vec<String> = argv;

    let PtyHandles { bridge, output_rx } = PtyBridge::spawn(SpawnOpts {
        argv: &argv_strings,
        cwd: Some(&workspace),
        env,
        cols: 120,
        rows: 32,
    })
    .with_context(|| format!("PtyBridge::spawn for {}", plugin.id))?;

    let input_tx = bridge.input_sender();
    let bridge = Arc::new(bridge);

    // Post-spawn readiness automation: auto-answer first-spawn confirmation
    // dialogs that would block a headless PTY (e.g. codex 0.130+'s "Hooks
    // need review" menu — the user can't reach the codex TUI to pick "2
    // Trust all and continue" because the dialog gates input). Steps are
    // declared in the plugin manifest's `ready_plan`, so claude (no such
    // dialog) ships an empty plan and this is `None`.
    let ready_plan = ReadyPlanRunner::from_plan(&plugin.ready_plan, input_tx.clone(), &agent_id);

    // Drain the PTY's output mpsc into the shared resume buffer. The pump
    // owns the receiver for the agent's whole lifetime — WS subscribers
    // read from the buffer, never the mpsc. EOF closes the buffer so any
    // WS writer task that's parked on `wait_changed` wakes up and exits.
    //
    // The pump is also where OSC lifecycle markers are scanned — exactly
    // once per agent, so multi-attach subscribers don't each redundantly
    // re-detect ShimReady/ShimExit, and resume attaches (whose cursor may
    // skip past the original OSC) can be told the current lifecycle via
    // Hello + the broadcast.
    let stream = Arc::new(PtyStream::new());
    let lifecycle = Arc::new(Mutex::new(Lifecycle::default()));
    // capacity=16 holds ShimReady + ShimExit plus headroom even if both
    // events fire faster than the slowest WS subscriber drains them.
    let (lifecycle_tx, _lifecycle_rx) = tokio::sync::broadcast::channel(16);
    {
        let stream = stream.clone();
        let lifecycle = lifecycle.clone();
        let lifecycle_tx = lifecycle_tx.clone();
        let agent_id_for_log = agent_id.clone();
        let recorder = recorder.clone();
        let mut ready_plan = ready_plan;
        // Continuous "alive but can't work" scanner (auth/quota banners) that
        // raises LifecycleEvent::HealthFail on first match. Built out here (not
        // inside the task) so it can read the borrowed `plugin`; the resolved
        // needles are owned and move into the pump alongside ready_plan.
        let mut health_scanner =
            HealthScanner::from_needles(&plugin.health_needles, lifecycle_tx.clone(), &agent_id);
        tokio::spawn(async move {
            let mut output_rx = output_rx;
            let mut osc_buf: Vec<u8> = Vec::new();
            while let Some(chunk) = output_rx.recv().await {
                scan_osc(&mut osc_buf, &chunk, &lifecycle, &lifecycle_tx);
                if let Some(rp) = ready_plan.as_mut() {
                    rp.scan(&chunk);
                }
                if let Some(hs) = health_scanner.as_mut() {
                    hs.scan(&chunk);
                }
                if let Some(rec) = &recorder {
                    rec.write_chunk(chunk.clone());
                }
                stream.append(chunk);
            }
            tracing::debug!(agent = %agent_id_for_log, "pty output drained, closing stream");
            stream.close();
            // Dropping `recorder` here is what signals EOF to the writer
            // task — every clone of the handle (including this pump's) is
            // gone, the mpsc closes, and `wait_finalize` resolves.
            drop(recorder);
        });
    }

    let slot = AgentSlot {
        channel: AgentChannel::Pty {
            bridge,
            stream,
            input_tx,
        },
        lifecycle,
        lifecycle_tx,
        cli: plugin.id.clone(),
        role: role.unwrap_or_else(|| plugin.id.clone()),
        workspace: workspace.to_string_lossy().into_owned(),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        // Starts false; flipped true when the agent's swarmx-mcp pings
        // /api/agent/:id/mcp-ready. We keep only the Sender — subscribers call
        // `.subscribe()`; `send_replace` updates the retained value even before
        // any subscriber exists, so an early ping is never lost.
        mcp_ready: tokio::sync::watch::channel(false).0,
        tui_http_port,
        // reasonix *and* zulu listen here; the discriminant is `live_delivery`.
        serve_http_port,
        zulu: zulu_conv.clone(),
        live_delivery: crate::input_delivery::LiveDelivery::at_spawn(
            plugin.input_delivery,
            tui_http_port,
            serve_http_port,
            zulu_conv,
            &workspace.to_string_lossy(),
        ),
    };

    Ok(AgentSpawn {
        agent_id,
        slot,
        transcript_session_id,
    })
}

fn enforce_billing_policy(plugin: &CliPlugin) -> Result<()> {
    crate::billing::enforce_spawn_billing_policy(plugin).map_err(anyhow::Error::msg)
}

/// Grab a free loopback TCP port by binding `127.0.0.1:0` and releasing it. Used
/// to assign opencode's TUI a known `--port` for the `/tui/*` HTTP control API.
/// There's a tiny TOCTOU window between release and opencode re-binding, but the
/// port space is large and the child binds within milliseconds.
fn alloc_ephemeral_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

fn env_blocked(plugin: &CliPlugin, key: &str) -> bool {
    plugin
        .blocked_env_prefixes
        .iter()
        .any(|p| !p.is_empty() && key.starts_with(p))
}

/// True iff `key` matches one of the prefixes the plugin itself declared in
/// `provider_env_prefixes`. spawn forwards ONLY declared prefixes (see the
/// security note at the call site): a full-shell agent must never inherit a
/// provider key its own CLI doesn't authenticate with.
fn provider_env_declared(plugin: &CliPlugin, key: &str) -> bool {
    plugin
        .provider_env_prefixes
        .iter()
        .any(|p| !p.is_empty() && key.starts_with(p))
}

/// The full forwarding decision for a provider-env candidate: declared by
/// this plugin AND not blocked by it AND not a Claude Code nesting marker.
/// Extracted as one function so the cross-plugin isolation matrix test
/// exercises the exact gate the spawn loop applies.
fn forward_provider_env(plugin: &CliPlugin, key: &str) -> bool {
    provider_env_declared(plugin, key)
        && !env_blocked(plugin, key)
        && !is_claude_code_nesting_marker(key)
}

/// Claude Code's own harness/nesting markers (CLAUDECODE, CLAUDE_CODE_*,
/// CLAUDE_EFFORT), which a spawned `claude` must NOT inherit. They are NOT
/// provider creds — they're harness markers an OUTER Claude Code sets, but
/// they happen to match the `CLAUDE_` provider prefix. If the swarmx server
/// was itself launched from inside a Claude Code session (or these are
/// exported globally), forwarding them makes the spawned `claude` believe it
/// is a NESTED child session: it then refuses to write its own
/// `~/.claude/projects/<cwd>/<id>.jsonl` transcript — which silently breaks
/// usage/cost collection for the captain (the tailer never finds a file) and
/// risks nested-session hangs. Strip them so every spawned claude is a clean
/// top-level run. `CLAUDECODE` lacks the underscore and wouldn't match the
/// prefix anyway, but is listed here as defense in case the prefix list
/// changes.
fn is_claude_code_nesting_marker(key: &str) -> bool {
    key == "CLAUDECODE" || key == "CLAUDE_EFFORT" || key.starts_with("CLAUDE_CODE")
}

/// Server-side OSC scanner. Identical algorithm to the M2 client-side
/// scanner in `pty_ws.rs`, lifted here so it runs exactly once per agent
/// regardless of how many WS subscribers are attached.
///
/// Updates `lifecycle` in place and broadcasts state-change events.
/// `osc_buf` carries state across calls so OSC sequences split across
/// `read(2)` boundaries are still matched.
fn scan_osc(
    osc_buf: &mut Vec<u8>,
    chunk: &[u8],
    lifecycle: &Mutex<Lifecycle>,
    tx: &tokio::sync::broadcast::Sender<LifecycleEvent>,
) {
    osc_buf.extend_from_slice(chunk);

    loop {
        // Bound growth; OSC sequences are short, anything beyond a few KB
        // means we're holding non-OSC junk indefinitely.
        if osc_buf.len() > 4096 {
            let keep_from = osc_buf.len() - 256;
            osc_buf.drain(..keep_from);
        }

        if let Some(pos) = find(osc_buf, OSC_READY) {
            osc_buf.drain(..pos + OSC_READY.len());
            let already = {
                let mut l = lifecycle.lock();
                let prev = l.shim_ready;
                l.shim_ready = true;
                prev
            };
            if !already {
                let _ = tx.send(LifecycleEvent::ShimReady);
            }
            continue;
        }
        if let Some(pos) = find(osc_buf, OSC_EXIT_PREFIX) {
            let after = pos + OSC_EXIT_PREFIX.len();
            if let Some(end_rel) = osc_buf[after..].iter().position(|&b| b == OSC_TERMINATOR) {
                let code_bytes = &osc_buf[after..after + end_rel];
                let code = std::str::from_utf8(code_bytes)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(-1);
                osc_buf.drain(..after + end_rel + 1);
                let new_exit = {
                    let mut l = lifecycle.lock();
                    if l.shim_exit.is_some() {
                        false
                    } else {
                        l.shim_exit = Some(code);
                        true
                    }
                };
                if new_exit {
                    let _ = tx.send(LifecycleEvent::ShimExit(code));
                }
                continue;
            } else {
                // OSC exit prefix matched but terminator not in this chunk;
                // wait for more bytes.
                break;
            }
        }
        break;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Continuous scanner for "alive but can't work" PTY banners (not logged in,
/// rate limited, invalid key), declared per CLI as [`crate::plugins::HealthNeedle`].
///
/// Unlike [`ReadyPlanRunner`] — an ordered, single-shot cursor that auto-answers
/// startup dialogs — every needle here is live at once and the whole scanner
/// **latches after the first match**: the banner repaints every frame, but one
/// `HealthFail` is enough to flip the UI to an honest failure card, so we never
/// spam the lifecycle channel. Needle matching reuses the same stitched-buffer
/// substring search as the ready plan, so a match split across two PTY chunks
/// (e.g. "Not logged" + " in") is still found.
struct HealthScanner {
    /// (needle bytes, human reason, coarse kind) resolved from the manifest.
    needles: Vec<(Vec<u8>, String, String)>,
    /// Sliding window over PTY output, bounded by `MAX_BUFFER`.
    buf: Vec<u8>,
    /// Latched true after the first match (or once the startup window closes)
    /// so we report a failure exactly once and then stop scanning.
    fired: bool,
    /// Hard deadline after which we stop scanning entirely. Auth/quota banners
    /// ("Not logged in · Run /login") are a STARTUP phenomenon — claude prints
    /// them within the first second or two of launch. Past this window the same
    /// substring in the PTY stream is almost certainly the model's own content
    /// (writing auth code, quoting a 401 body, cat-ing a file, or discussing
    /// login), NOT a real failure. Without the window, a working agent that
    /// merely MENTIONS "Not logged in" would be flipped to a false failure. The
    /// recovery path (tailer clears the error on the next real activity) is the
    /// safety net for anything that still slips through inside the window.
    deadline: Instant,
    /// The agent's lifecycle broadcast — the same channel ShimReady/ShimExit
    /// ride, so the existing subscriber in `spawn_with_bookkeeping` picks the
    /// HealthFail up and publishes AgentState::Error without new plumbing.
    tx: tokio::sync::broadcast::Sender<LifecycleEvent>,
    /// Agent id for log lines only.
    agent_id: String,
}

impl HealthScanner {
    const MAX_BUFFER: usize = 8 * 1024;
    /// Startup window during which auth/quota banners are trusted as real. A
    /// genuine failure repaints its banner every frame, so it's caught within
    /// the first second; this just bounds the false-positive surface.
    const SCAN_WINDOW: Duration = Duration::from_secs(45);

    /// Build from a plugin's `health_needles`. Returns `None` when there are no
    /// usable needles, so the pump skips health scanning entirely (the common
    /// case for CLIs that declare none).
    fn from_needles(
        needles: &[crate::plugins::HealthNeedle],
        tx: tokio::sync::broadcast::Sender<LifecycleEvent>,
        agent_id: &str,
    ) -> Option<Self> {
        let needles: Vec<(Vec<u8>, String, String)> = needles
            .iter()
            .filter(|n| !n.needle.is_empty())
            .map(|n| {
                (
                    n.needle.clone().into_bytes(),
                    n.reason.clone(),
                    n.kind.clone(),
                )
            })
            .collect();
        if needles.is_empty() {
            return None;
        }
        Some(Self {
            needles,
            buf: Vec::with_capacity(2048),
            fired: false,
            deadline: Instant::now() + Self::SCAN_WINDOW,
            tx,
            agent_id: agent_id.to_string(),
        })
    }

    fn scan(&mut self, chunk: &[u8]) {
        if self.fired {
            return;
        }
        // Past the startup window, stop scanning: any "Not logged in" now is
        // almost certainly the model's own output, not a credential banner.
        if Instant::now() > self.deadline {
            self.fired = true;
            self.buf = Vec::new();
            return;
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > Self::MAX_BUFFER {
            let keep_from = self.buf.len() - 1024;
            self.buf.drain(..keep_from);
        }
        for (needle, reason, kind) in &self.needles {
            if find(&self.buf, needle).is_some() {
                self.fired = true;
                tracing::warn!(
                    agent = %self.agent_id, reason = %reason, kind = %kind,
                    "health: CLI reported it cannot work (auth/quota); raising Error",
                );
                // Subscriber may not exist yet under pathological timing; a
                // dropped send just means no Error event, which is no worse
                // than the pre-existing silent failure. Don't block the pump.
                let _ = self.tx.send(LifecycleEvent::HealthFail {
                    reason: reason.clone(),
                    kind: kind.clone(),
                });
                self.buf.clear();
                return;
            }
        }
    }
}

fn ensure_workspace(root: &Path, agent_id: &str) -> Result<PathBuf> {
    let dir = root.join(agent_id);
    std::fs::create_dir_all(&dir).with_context(|| format!("create workspace {}", dir.display()))?;
    // Resolve symlinks so the path matches what the spawned CLI sees after
    // its own canonicalize step. On macOS `/tmp` is a symlink to
    // `/private/tmp`, and claude / codex both trust by canonical path —
    // without this, our trust-patch key wouldn't match and the prompt
    // resurfaces.
    let canonical = std::fs::canonicalize(&dir)
        .with_context(|| format!("canonicalize workspace {}", dir.display()))?;
    Ok(canonical)
}

/// Shared-workspace variant: the caller supplies the final directory
/// (typically `<workspaces_root>/spell-<uuid>/` or a user-supplied
/// monorepo path). We `create_dir_all` + canonicalize for the same
/// trust-by-canonical-path reason as `ensure_workspace`.
fn ensure_shared_workspace(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create shared workspace {}", dir.display()))?;
    let canonical = std::fs::canonicalize(dir)
        .with_context(|| format!("canonicalize shared workspace {}", dir.display()))?;
    Ok(canonical)
}

/// Find `swarmx-shim` next to the current executable. Falls back to
/// `target/debug/swarmx-shim` relative to the manifest dir during
/// `cargo run`, since `current_exe` points into `target/debug/deps/...`
/// for tests but `target/debug/` for `cargo run`.
pub fn locate_shim() -> Result<PathBuf> {
    locate_sibling_bin("swarmx-shim", "SWARMX_SHIM_PATH")
}

/// Find `swarmx-mcp` next to the current executable (same heuristic as
/// `locate_shim`). The path is baked into MCP config entries so claude /
/// codex can launch it directly.
pub fn locate_mcp() -> Result<PathBuf> {
    locate_sibling_bin("swarmx-mcp", "SWARMX_MCP_PATH")
}

/// Host-side post-spawn readiness automation, built from a plugin's
/// `ready_plan` ([`crate::plugins::ReadyStep`]). A **sequential** golutra-style
/// state machine: the host advances a `cursor` through the steps in declared
/// order as PTY output arrives, fed by `scan(chunk)` from the pump. Step kinds:
///   - `answer_dialog` — wait for `needle`, inject `response`, advance (the
///     codex "Hooks need review" → `2\r` case; the only kind any shipped CLI
///     uses today).
///   - `wait_for` — block until `needle` appears, then advance (gate a later
///     step on a prompt/banner instead of a fixed sleep).
///   - `input` — inject `response` immediately when this step becomes active.
///   - `extract_session_id` — capture the token after `needle` into the named
///     `into` slot (resume support), then advance.
///
/// This is the **data-driven replacement** for the old hard-coded
/// `DialogAutoAnswer`: needle/response live in `cli-plugins/<id>.toml`, so a CLI
/// declares its own onboarding with zero Rust change. Safety constraints
/// preserved: each step is single-shot (cursor only moves forward), needle
/// steps are time-boxed (`window_ms`, default 30s — the plan advances past a
/// step whose needle never shows), needle matching is the literal substring
/// (never a short fragment), and the output buffer is bounded (sliding 8 KiB,
/// so a chatty agent can't OOM us). Host-side, so it works regardless of which
/// UI (if any) is attached. NOTE: advance is output-driven (only fires while
/// the pump delivers chunks); a step waiting on a needle that the CLL never
/// prints just times out and the plan moves on.
struct ReadyPlanRunner {
    steps: Vec<PlanStep>,
    /// Index of the currently-active step; `>= steps.len()` once the plan is done.
    cursor: usize,
    /// Deadline for the active needle step (`None` for `input`, which fires
    /// immediately, and once the plan is done). Set when a step becomes active.
    deadline: Option<Instant>,
    /// Sliding window over PTY output, shared across steps. Bounded by `MAX_BUFFER`.
    buf: Vec<u8>,
    /// Cloned PtyBridge input channel. `try_send` is non-blocking and used from
    /// the sync `scan` path; ample capacity for a few bytes, degrade silently.
    input_tx: mpsc::Sender<Bytes>,
    /// Agent_id for log lines only — never written into the PTY.
    agent_id: String,
    /// `extract_session_id` captures, keyed by the step's `into` name.
    captured: std::collections::HashMap<String, String>,
}

/// One resolved step (bytes pre-decoded from the manifest strings).
struct PlanStep {
    kind: crate::plugins::ReadyStepKind,
    needle: Vec<u8>,
    response: Vec<u8>,
    window: Duration,
    into: String,
}

impl ReadyPlanRunner {
    const MAX_BUFFER: usize = 8 * 1024;

    /// Build from a plugin's `ready_plan`. Returns `None` when there are no
    /// actionable steps, so the PTY pump skips scanning entirely (the common
    /// case — e.g. claude has no blocking dialog and ships an empty plan).
    /// Invalid steps (missing needle/response/into for their kind) are
    /// warn-skipped rather than aborting the whole plan.
    fn from_plan(
        plan: &[crate::plugins::ReadyStep],
        input_tx: mpsc::Sender<Bytes>,
        agent_id: &str,
    ) -> Option<Self> {
        use crate::plugins::ReadyStepKind;
        let mut steps = Vec::new();
        for step in plan {
            let needs_needle = matches!(
                step.kind,
                ReadyStepKind::AnswerDialog
                    | ReadyStepKind::WaitFor
                    | ReadyStepKind::ExtractSessionId
            );
            let needs_response = matches!(
                step.kind,
                ReadyStepKind::AnswerDialog | ReadyStepKind::Input
            );
            if needs_needle && step.needle.is_empty() {
                tracing::warn!(agent = %agent_id, kind = ?step.kind, "ready_plan: step missing needle; skipping");
                continue;
            }
            if needs_response && step.response.is_empty() {
                tracing::warn!(agent = %agent_id, kind = ?step.kind, "ready_plan: step missing response; skipping");
                continue;
            }
            if matches!(step.kind, ReadyStepKind::ExtractSessionId) && step.into.is_empty() {
                tracing::warn!(agent = %agent_id, "ready_plan: extract_session_id missing `into`; skipping");
                continue;
            }
            steps.push(PlanStep {
                kind: step.kind,
                needle: step.needle.clone().into_bytes(),
                response: step.response.clone().into_bytes(),
                window: Duration::from_millis(step.window_ms),
                into: step.into.clone(),
            });
        }
        if steps.is_empty() {
            return None;
        }
        let mut runner = Self {
            steps,
            cursor: 0,
            deadline: None,
            buf: Vec::with_capacity(2048),
            input_tx,
            agent_id: agent_id.to_string(),
            captured: std::collections::HashMap::new(),
        };
        runner.arm_deadline(Instant::now());
        Some(runner)
    }

    /// Captured `extract_session_id` values (slot name → token). A future
    /// resume path reads this; today it's surfaced for logging/tests.
    #[allow(dead_code)]
    fn captured(&self) -> &std::collections::HashMap<String, String> {
        &self.captured
    }

    /// Set `deadline` for the step now under `cursor`: a window for needle
    /// steps, `None` for `input` (fires immediately) and for "plan done".
    fn arm_deadline(&mut self, now: Instant) {
        use crate::plugins::ReadyStepKind;
        self.deadline = self.steps.get(self.cursor).and_then(|s| match s.kind {
            ReadyStepKind::Input => None,
            _ => Some(now + s.window),
        });
    }

    fn inject(&self, bytes: &[u8]) {
        // try_send is non-blocking; if the channel is full something is very
        // wrong but it's not worth blocking the PTY pump for.
        match self.input_tx.try_send(Bytes::copy_from_slice(bytes)) {
            Ok(()) => tracing::info!(agent = %self.agent_id, "ready_plan: injected step response"),
            Err(err) => tracing::warn!(
                agent = %self.agent_id, ?err,
                "ready_plan: inject try_send failed; user may need to act manually",
            ),
        }
    }

    fn scan(&mut self, chunk: &[u8]) {
        if self.cursor >= self.steps.len() {
            return; // plan complete
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > Self::MAX_BUFFER {
            let keep_from = self.buf.len() - 1024;
            self.buf.drain(..keep_from);
        }
        self.advance();
        if self.cursor >= self.steps.len() {
            self.buf.clear();
        }
    }

    /// Progress the cursor as far as the current buffer allows. Stops at the
    /// first needle step whose needle hasn't appeared yet (and hasn't timed
    /// out). Each iteration clones the step's small byte fields so we can then
    /// mutate `self` (cursor / captured / inject) without an aliasing borrow.
    fn advance(&mut self) {
        use crate::plugins::ReadyStepKind;
        while self.cursor < self.steps.len() {
            let now = Instant::now();
            if let Some(dl) = self.deadline {
                if now > dl {
                    // An `answer_dialog` step auto-answers a startup dialog IF it
                    // appears — a timeout just means the dialog never showed,
                    // which is the NORMAL case, not a fault. e.g. codex's "Hooks
                    // need review" prompt is suppressed entirely when we launch
                    // with `--dangerously-bypass-hook-trust` (codex >=0.131), so
                    // that step times out on every spawn. Log it at debug. A
                    // gating `wait_for` / `extract_session_id` needle that never
                    // shows IS a real readiness gap → keep that at warn.
                    if self.steps[self.cursor].kind == ReadyStepKind::AnswerDialog {
                        tracing::debug!(
                            agent = %self.agent_id, step = self.cursor,
                            "ready_plan: answer_dialog needle never appeared (dialog not shown); advancing",
                        );
                    } else {
                        tracing::warn!(
                            agent = %self.agent_id, step = self.cursor,
                            "ready_plan: step timed out waiting for its needle; advancing",
                        );
                    }
                    self.cursor += 1;
                    self.arm_deadline(now);
                    continue;
                }
            }
            let (kind, needle, response, into) = {
                let s = &self.steps[self.cursor];
                (s.kind, s.needle.clone(), s.response.clone(), s.into.clone())
            };
            match kind {
                ReadyStepKind::Input => {
                    self.inject(&response);
                    self.cursor += 1;
                    self.arm_deadline(now);
                }
                ReadyStepKind::AnswerDialog => {
                    if find(&self.buf, &needle).is_some() {
                        self.inject(&response);
                        self.cursor += 1;
                        self.arm_deadline(now);
                    } else {
                        break;
                    }
                }
                ReadyStepKind::WaitFor => {
                    if find(&self.buf, &needle).is_some() {
                        self.cursor += 1;
                        self.arm_deadline(now);
                    } else {
                        break;
                    }
                }
                ReadyStepKind::ExtractSessionId => {
                    if let Some(pos) = find(&self.buf, &needle) {
                        let token = extract_token_after(&self.buf, pos + needle.len());
                        tracing::info!(
                            agent = %self.agent_id, into = %into, value = %token,
                            "ready_plan: captured session id",
                        );
                        self.captured.insert(into, token);
                        self.cursor += 1;
                        self.arm_deadline(now);
                    } else {
                        break;
                    }
                }
            }
        }
    }
}

/// Take the first whitespace-delimited token starting at byte `start` in `buf`
/// (skipping leading spaces/tabs), lossy-decoded. Used by `extract_session_id`
/// to grab the value printed right after a marker like `Session id:`.
fn extract_token_after(buf: &[u8], start: usize) -> String {
    let tail = buf.get(start..).unwrap_or(&[]);
    let trimmed: &[u8] = {
        let lead = tail
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();
        &tail[lead..]
    };
    let end = trimmed
        .iter()
        .position(|b| b.is_ascii_whitespace())
        .unwrap_or(trimmed.len());
    String::from_utf8_lossy(&trimmed[..end]).into_owned()
}

#[cfg(test)]
mod model_overlay_tests {
    use super::model_overlay_args;

    #[test]
    fn substitutes_placeholder() {
        let tmpl = vec!["--model".to_string(), "{model}".to_string()];
        assert_eq!(
            model_overlay_args("opus", &tmpl),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn substitutes_inside_a_joined_arg() {
        // A CLI that takes `--model=<v>` as one token still works.
        let tmpl = vec!["--model={model}".to_string()];
        assert_eq!(model_overlay_args("sonnet", &tmpl), vec!["--model=sonnet"]);
    }

    #[test]
    fn empty_template_yields_no_args() {
        assert!(model_overlay_args("opus", &[]).is_empty());
    }
}

#[cfg(test)]
mod locate_tests {
    use super::which_in;

    #[test]
    fn finds_a_file_on_path() {
        let dir = std::env::temp_dir().join(format!("swarmx-which-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("swarmx-fake-bin");
        std::fs::write(&f, b"x").unwrap();
        // Pass PATH explicitly — never touch the process-global env var, which
        // would race the other parallel tests (the old set_var made CI flaky).
        let found = which_in("swarmx-fake-bin", dir.as_os_str());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found, Some(f));
    }

    #[test]
    fn returns_none_for_absent() {
        let r = which_in(
            "swarmx-definitely-not-a-real-binary-xyz",
            std::env::temp_dir().as_os_str(),
        );
        assert_eq!(r, None);
    }
}

#[cfg(test)]
mod ready_plan_tests {
    use super::*;
    use crate::plugins::{ReadyStep, ReadyStepKind};

    fn step(kind: ReadyStepKind, needle: &str, response: &str, into: &str) -> ReadyStep {
        ReadyStep {
            kind,
            needle: needle.into(),
            response: response.into(),
            window_ms: 30_000,
            into: into.into(),
        }
    }
    fn answer(needle: &str, response: &str) -> ReadyStep {
        step(ReadyStepKind::AnswerDialog, needle, response, "")
    }
    fn codex_hooks_step() -> ReadyStep {
        answer("Hooks need review", "2\r")
    }

    fn make_pair() -> (ReadyPlanRunner, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let runner = ReadyPlanRunner::from_plan(&[codex_hooks_step()], tx, "codex-test")
            .expect("non-empty plan yields a runner");
        (runner, rx)
    }

    #[tokio::test]
    async fn empty_plan_is_none() {
        let (tx, _rx) = mpsc::channel::<Bytes>(8);
        assert!(
            ReadyPlanRunner::from_plan(&[], tx, "x").is_none(),
            "no steps ⇒ no runner (pump skips scanning)",
        );
    }

    #[tokio::test]
    async fn sends_response_on_match() {
        let (mut r, mut rx) = make_pair();
        r.scan(b"some noise before the dialog\n");
        r.scan(b"\nHooks need review\n1 hook is new\n");
        // Should have sent "2\r" exactly once, then advanced past the step.
        let got = rx.try_recv().expect("response should have been queued");
        assert_eq!(&got[..], b"2\r");
        assert!(rx.try_recv().is_err(), "no second response");
        assert_eq!(r.cursor, 1, "cursor advanced past the only step");
    }

    #[tokio::test]
    async fn single_shot_after_fired() {
        let (mut r, mut rx) = make_pair();
        r.scan(b"Hooks need review");
        let _ = rx.try_recv().expect("first response sent");
        // Cursor already moved past; a repeat of the dialog text is ignored.
        r.scan(b"Hooks need review again somehow");
        assert!(
            rx.try_recv().is_err(),
            "second match must NOT enqueue another response",
        );
    }

    #[tokio::test]
    async fn matches_across_chunk_boundary() {
        let (mut r, mut rx) = make_pair();
        // Split the needle across two chunks — the sliding buffer must
        // stitch them back together before matching.
        r.scan(b"Hooks ne");
        assert!(rx.try_recv().is_err(), "no premature match");
        r.scan(b"ed review");
        let got = rx.try_recv().expect("match after stitching chunks");
        assert_eq!(&got[..], b"2\r");
    }

    #[tokio::test]
    async fn ignores_unrelated_substrings() {
        let (mut r, mut rx) = make_pair();
        r.scan(b"Trust this folder? hook count: 0");
        r.scan(b"reviewing your code changes now");
        assert!(
            rx.try_recv().is_err(),
            "substring 'hook' / 'review' alone must NOT trigger"
        );
        assert_eq!(r.cursor, 0, "still waiting on the real needle");
    }

    #[tokio::test]
    async fn does_not_fire_after_window_expires() {
        let (mut r, mut rx) = make_pair();
        // Synthesize an expired deadline so we don't actually sleep 30s.
        r.deadline = Some(Instant::now() - Duration::from_secs(1));
        r.scan(b"Hooks need review");
        assert!(rx.try_recv().is_err(), "expired window must not fire");
        assert_eq!(r.cursor, 1, "timed-out step is skipped, not fired");
    }

    #[tokio::test]
    async fn buffer_stays_bounded_under_chatty_input() {
        let (mut r, _rx) = make_pair();
        // Push many small chunks of unrelated bytes.
        for _ in 0..200 {
            r.scan(&[b'.'; 1024]);
        }
        assert!(
            r.buf.len() <= ReadyPlanRunner::MAX_BUFFER,
            "buffer must stay capped (got {})",
            r.buf.len(),
        );
    }

    #[tokio::test]
    async fn multiple_dialogs_each_fire_once() {
        let (tx, mut rx) = mpsc::channel::<Bytes>(8);
        let plan = vec![answer("Trust this folder", "1\r"), codex_hooks_step()];
        let mut r = ReadyPlanRunner::from_plan(&plan, tx, "multi").expect("runner");
        // Sequential: the dialogs are declared (and here arrive) in order.
        r.scan(b"? Trust this folder ? [y/N]");
        assert_eq!(&rx.try_recv().expect("first dialog answered")[..], b"1\r");
        r.scan(b"... later ... Hooks need review ...");
        assert_eq!(&rx.try_recv().expect("second dialog answered")[..], b"2\r");
        assert!(rx.try_recv().is_err(), "no extra responses");
        assert_eq!(r.cursor, 2, "both steps consumed");
    }

    #[tokio::test]
    async fn wait_for_then_input_is_sequential() {
        // wait_for gates the input: nothing is typed until the banner shows.
        let (tx, mut rx) = mpsc::channel::<Bytes>(8);
        let plan = vec![
            step(ReadyStepKind::WaitFor, "READY", "", ""),
            step(ReadyStepKind::Input, "", "go\r", ""),
        ];
        let mut r = ReadyPlanRunner::from_plan(&plan, tx, "seq").expect("runner");
        r.scan(b"booting...\n");
        assert!(
            rx.try_recv().is_err(),
            "input must NOT fire before the wait_for matches"
        );
        assert_eq!(r.cursor, 0);
        r.scan(b"all systems READY now\n");
        // wait_for matched → cursor advances → input fires immediately.
        assert_eq!(
            &rx.try_recv().expect("input injected after wait")[..],
            b"go\r"
        );
        assert_eq!(r.cursor, 2, "plan complete");
    }

    #[tokio::test]
    async fn input_first_fires_immediately() {
        let (tx, mut rx) = mpsc::channel::<Bytes>(8);
        let plan = vec![step(ReadyStepKind::Input, "", "hi\r", "")];
        let mut r = ReadyPlanRunner::from_plan(&plan, tx, "in").expect("runner");
        r.scan(b"any output at all");
        assert_eq!(
            &rx.try_recv().expect("input fired on first scan")[..],
            b"hi\r"
        );
    }

    #[tokio::test]
    async fn extract_session_id_captures_token() {
        let (tx, _rx) = mpsc::channel::<Bytes>(8);
        let plan = vec![step(
            ReadyStepKind::ExtractSessionId,
            "Session id:",
            "",
            "sid",
        )];
        let mut r = ReadyPlanRunner::from_plan(&plan, tx, "ex").expect("runner");
        r.scan(b"banner\nSession id:  abc-123-def \ntrailing output\n");
        assert_eq!(
            r.captured().get("sid").map(String::as_str),
            Some("abc-123-def")
        );
        assert_eq!(r.cursor, 1, "advanced after capture");
    }

    #[test]
    fn extract_token_after_skips_leading_space_and_stops_at_whitespace() {
        assert_eq!(extract_token_after(b"   tok-99\nrest", 0), "tok-99");
        assert_eq!(extract_token_after(b"x", 1), ""); // start at EOF → empty
    }
}

#[cfg(test)]
mod billing_policy_tests {
    use super::*;
    use crate::plugins::{
        BillingSurface, InputDelivery, McpFormat, ReadyStep, StopHookFormat, TrustFormat,
    };

    #[test]
    fn forwarded_env_keeps_macos_keychain_vars() {
        // USER / LOGNAME are load-bearing: macOS Keychain (claude's OAuth token
        // store) resolves the login keychain via $USER. They were dropped from
        // this spawn allowlist once, silently breaking login. Pin them so a
        // future edit to FORWARDED_ENV_KEYS can't regress it unnoticed.
        assert!(
            FORWARDED_ENV_KEYS.contains(&"USER"),
            "USER must stay forwarded (macOS Keychain / claude login)"
        );
        assert!(
            FORWARDED_ENV_KEYS.contains(&"LOGNAME"),
            "LOGNAME must stay forwarded alongside USER"
        );
    }

    #[test]
    fn claude_code_nesting_markers_are_excluded_from_forwarding() {
        // These flip a spawned `claude` into nested-child mode, where it stops
        // writing its session transcript — which silently breaks usage/cost
        // collection for the captain. They match the `CLAUDE_` provider prefix,
        // so without an explicit exclusion they'd leak whenever the server was
        // launched from inside a Claude Code session.
        for marker in [
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_EFFORT",
        ] {
            assert!(
                is_claude_code_nesting_marker(marker),
                "{marker} must be stripped, not forwarded to the spawned CLI"
            );
        }
        // Real provider creds/config must STILL pass the prefix forward.
        for cred in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CONFIG_DIR",
            "DEEPSEEK_API_KEY",
        ] {
            assert!(
                !is_claude_code_nesting_marker(cred),
                "{cred} is real config and must keep being forwarded"
            );
        }
    }

    fn minimal_plugin(id: &str) -> CliPlugin {
        CliPlugin {
            id: id.to_string(),
            display_name: id.to_string(),
            binary: id.to_string(),
            default_args: Vec::new(),
            home_env: "HOME".to_string(),
            billing_surface: BillingSurface::Unknown,
            requires_explicit_billing_opt_in: false,
            blocked_env_prefixes: Vec::new(),
            provider_env_prefixes: Vec::new(),
            auto_trust_workspace: false,
            auto_dismiss_update: false,
            auto_inject_mcp: false,
            auto_inject_stop_hook: false,
            ready_plan: Vec::<ReadyStep>::new(),
            health_needles: Vec::new(),
            trust_format: TrustFormat::None,
            mcp_format: McpFormat::None,
            stop_hook_format: StopHookFormat::None,
            stop_hook_timeout: 10_000,
            input_settle_ms: 0,
            model_args: Vec::new(),
            default_model: None,
            native_tiers: false,
            effort_args: Vec::new(),
            effort_levels: HashMap::new(),
            input_delivery: InputDelivery::Keystroke,
            bracketed_paste: false,
            bootstrap_ready_needle: String::new(),
            bootstrap_ready_settle_ms: 0,
        }
    }

    #[test]
    fn blocks_configured_env_prefixes() {
        let mut p = minimal_plugin("claude");
        p.blocked_env_prefixes = vec!["ANTHROPIC_".into()];
        assert!(env_blocked(&p, "ANTHROPIC_API_KEY"));
        assert!(env_blocked(&p, "ANTHROPIC_BASE_URL"));
        assert!(!env_blocked(&p, "OPENAI_API_KEY"));
    }

    /// Spawn forwards a provider env var only when the plugin's own manifest
    /// declares the prefix — unit-level pin of the per-plugin scoping before
    /// the shipped-manifest matrix below.
    #[test]
    fn provider_env_requires_plugin_declaration() {
        let mut p = minimal_plugin("reasonix");
        // Undeclared: nothing crosses, even a prefix some OTHER plugin uses.
        assert!(!forward_provider_env(&p, "DEEPSEEK_API_KEY"));
        assert!(!forward_provider_env(&p, "OPENAI_API_KEY"));
        // Declared: crosses.
        p.provider_env_prefixes = vec!["DEEPSEEK_".into()];
        assert!(forward_provider_env(&p, "DEEPSEEK_API_KEY"));
        assert!(!forward_provider_env(&p, "ANTHROPIC_API_KEY"));
        // Declared + blocked: the block still wins (the claude/ANTHROPIC_
        // billing-red-line shape).
        p.blocked_env_prefixes = vec!["DEEPSEEK_".into()];
        assert!(!forward_provider_env(&p, "DEEPSEEK_API_KEY"));
    }

    /// SECURITY matrix over the compiled-in shipped manifests: every CLI here
    /// is a full-shell agent, so a provider key in its env is plaintext handed
    /// to the model. Pin that `ANTHROPIC_*` reaches NO child (claude declares
    /// the prefix only to keep an explicit user opt-in reachable — the default
    /// block wins), `DEEPSEEK_*` reaches reasonix ONLY, and `OPENAI_*` reaches
    /// codex ONLY.
    #[test]
    fn provider_env_isolation_matrix_across_shipped_plugins() {
        let reg = crate::plugins::PluginRegistry::builtin();
        let get = |id: &str| reg.get(id).unwrap_or_else(|| panic!("{id} plugin present"));
        let all = ["claude", "codex", "opencode", "reasonix", "zulu", "kimi"];

        for id in all {
            assert!(
                !forward_provider_env(get(id), "ANTHROPIC_API_KEY"),
                "ANTHROPIC_API_KEY must not enter the {id} child"
            );
            assert!(
                !forward_provider_env(get(id), "ANTHROPIC_BASE_URL"),
                "ANTHROPIC_BASE_URL must not enter the {id} child"
            );
        }
        for id in all {
            let expect = id == "reasonix";
            assert_eq!(
                forward_provider_env(get(id), "DEEPSEEK_API_KEY"),
                expect,
                "DEEPSEEK_API_KEY reaches reasonix only, not {id}"
            );
        }
        for id in all {
            let expect = id == "codex";
            assert_eq!(
                forward_provider_env(get(id), "OPENAI_API_KEY"),
                expect,
                "OPENAI_API_KEY reaches codex only, not {id}"
            );
        }
        // claude keeps its own CLAUDE_ config family, minus the nesting
        // markers (those flip a spawned claude into transcript-suppressing
        // nested-child mode).
        assert!(forward_provider_env(get("claude"), "CLAUDE_CONFIG_DIR"));
        assert!(!forward_provider_env(
            get("claude"),
            "CLAUDE_CODE_ENTRYPOINT"
        ));
    }

    #[test]
    fn paid_sdk_surface_requires_opt_in_when_declared() {
        std::env::remove_var("SWARMX_ALLOW_PAID_TRANSPORT");
        let mut p = minimal_plugin("claude-sdk");
        p.billing_surface = BillingSurface::AgentSdkCredits;
        p.requires_explicit_billing_opt_in = true;
        let err = enforce_billing_policy(&p).expect_err("paid SDK surface must be rejected");
        assert!(err.to_string().contains("requires explicit opt-in"));
    }
}

/// L5c — substitute `{model}` in each manifest `model_args` entry. Pure +
/// unit-tested so the spawn argv path stays trivial. Caller decides whether a
/// model is in effect; this only renders the template.
fn model_overlay_args(model: &str, template: &[String]) -> Vec<String> {
    template
        .iter()
        .map(|a| a.replace("{model}", model))
        .collect()
}

/// Substitute the concrete effort value into a CLI's `effort_args` template
/// (`{effort}` placeholder). Mirrors `model_overlay_args`.
fn effort_overlay_args(effort: &str, template: &[String]) -> Vec<String> {
    template
        .iter()
        .map(|a| a.replace("{effort}", effort))
        .collect()
}

fn locate_sibling_bin(name: &str, env_override: &str) -> Result<PathBuf> {
    if let Ok(p) = std::env::var(env_override) {
        return Ok(PathBuf::from(p));
    }
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    // Candidate chain (first existing wins):
    //   1. next to current_exe — the installed/sidecar layout + `target/<profile>/`.
    //   2. one level up — `current_exe` is `target/<profile>/deps/<exe>` under
    //      `cargo run`/tests, where the sibling bin lives in `target/<profile>/`.
    //      (The old impl only checked #1, contradicting its own doc comment.)
    //   3. PATH — installed via `cargo install` / brew / packaged.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(&file));
            if let Some(up) = dir.parent() {
                candidates.push(up.join(&file));
            }
        }
    }
    for cand in &candidates {
        if cand.is_file() {
            return Ok(cand.clone());
        }
    }
    if let Some(p) = which_in_path(&file) {
        return Ok(p);
    }
    anyhow::bail!(
        "{name} not found (looked next to swarmx-server, one level up, and on PATH). \
         Build it with `cargo build -p {name}` or set {env_override}"
    )
}

/// First `file` found on `$PATH`, if any. Lets an installed (cargo install /
/// brew / packaged) sidecar binary be located when it's not next to the server.
fn which_in_path(file: &str) -> Option<PathBuf> {
    which_in(file, &std::env::var_os("PATH")?)
}

/// PATH lookup against an EXPLICIT path value — pure, so tests can exercise it
/// without mutating the process-global `PATH` env var. That mutation races
/// other tests under cargo's parallel runner (set_var is process-wide), and the
/// resulting flakiness took down CI on an unrelated commit.
fn which_in(file: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        let cand = dir.join(file);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}
