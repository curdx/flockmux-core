//! Per-CLI **pre-spawn** adapters (honest scope — see CONTEXT.md `EngineAdapter`).
//!
//! swarmx drives several coding CLIs — claude, codex, opencode, reasonix, zulu,
//! kimi — through one spawn pipeline. What lives here is ONLY what those CLIs
//! disagree on **before** the process is up:
//!   - where per-workspace trust is recorded,
//!   - how the swarmx-swarm MCP server is injected,
//!   - how the wake / stop hook is installed,
//!   - argv / env tweaks for the PTY launch,
//!   - optional transcript session id pinning.
//!
//! What does **NOT** live here (do not pretend otherwise):
//!   - live turn delivery (PTY paste / TUI HTTP / serve HTTP) →
//!     [`crate::input_delivery`] (TurnDelivery);
//!   - serve / TUI port allocation → [`crate::spawn`] matching
//!     [`crate::plugins::InputDelivery`];
//!   - reasonix / zulu / opencode drivers → `reasonix_serve` / `zulu_serve` /
//!     `opencode_tui`.
//!
//! The GENERIC machinery (PTY pump, ready-plan dialog answerer, health scanner,
//! model/effort overlay) lives in `spawn.rs` and asks this adapter only at the
//! pre-spawn seams above.
//!
//! Adding a CLI: drop a `cli/<name>.rs` implementing [`CliAdapter`], ensure
//! [`adapter_for`] routes its formats, and ship a `cli-plugins/<name>.toml`.
//! Live delivery knobs go in the plugin's `input_delivery` field + TurnDelivery,
//! not in this trait.

mod shared;

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod reasonix;
pub mod zulu;
pub mod kimi;

use crate::plugins::{CliPlugin, McpFormat, TrustFormat};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Per-spawn context the host computes once and threads into
/// [`CliAdapter::pre_spawn`]. Everything else an adapter needs comes from the
/// plugin manifest + workspace.
#[derive(Debug, Clone)]
pub struct PreSpawnCtx {
    /// The agent_id swarmx-server allocated for this spawn.
    pub agent_id: String,
    /// Absolute path to `swarmx-mcp` — baked into the per-spawn MCP entry so
    /// the path doesn't drift with the user's CWD or PATH.
    pub mcp_bin: PathBuf,
    /// Base URL of the swarmx-server REST API the MCP subprocess will talk to.
    /// Loopback today, but the field exists so a remote pairing mode doesn't need
    /// a schema change.
    pub server_url: String,
}

/// One CLI's **pre-spawn** behavior. Each method is a SEAM where CLIs differ at
/// launch time; a default no-op means "this CLI doesn't need that seam".
/// Implementors live in sibling modules and never reference each other.
///
/// Not a full engine facade: turn delivery and serve drivers are outside this
/// trait (see module docs).
pub trait CliAdapter: Send + Sync {
    /// Stable id for logs (matches the canonical plugin id of this CLI family).
    fn name(&self) -> &'static str;

    /// Host-side pre-spawn patches: pre-accept trust prompts, dismiss update
    /// nags, inject the swarmx-swarm MCP server, install the wake hook. Each
    /// capability is gated on the plugin's matching `auto_*` flag. Failures are
    /// logged at `warn!` but never propagated — at worst the user sees a prompt
    /// we tried to suppress (or the agent lacks swarm tools), which is annoying
    /// but not fatal.
    fn pre_spawn(&self, plugin: &CliPlugin, workspace: &Path, ctx: &PreSpawnCtx);

    /// argv tokens appended after the base args + model/effort overlay, on the
    /// PTY spawn path. Default: nothing.
    fn contribute_argv(&self, _plugin: &CliPlugin, _agent_id: &str, _argv: &mut Vec<String>) {}

    /// Extra env entries beyond the shared allowlist (e.g. an isolated per-agent
    /// config dir like codex's CODEX_HOME or opencode's OPENCODE_CONFIG). Runs
    /// after the generic env is built, before the PTY spawn. Default: nothing.
    fn contribute_env(
        &self,
        _plugin: &CliPlugin,
        _agent_id: &str,
        _env: &mut HashMap<String, String>,
    ) {
    }

    /// Some CLIs (claude) pin a known session id at spawn so the transcript
    /// tailer can locate the exact JSONL instead of guessing the newest file.
    /// The impl pushes any argv it needs and returns the id for `AgentSpawn`.
    /// Only consulted on the PTY path. Default: none.
    fn transcript_session_id(
        &self,
        _plugin: &CliPlugin,
        _agent_id: &str,
        _argv: &mut Vec<String>,
    ) -> Option<String> {
        None
    }
}

/// Resolve the adapter for a plugin from its declared config FORMATS — NOT its
/// literal id — so a CLI that reuses an existing family's formats (e.g. a
/// `gemini.toml` that writes claude-style config) gets that family's behavior
/// with zero new Rust. Coherent manifests (the only kind that work; guarded by
/// `plugins::tests::shipped_manifests_declare_formats`) map cleanly to one
/// family. An unknown / none combination falls to [`GenericAdapter`], which
/// no-ops loudly so a misconfigured plugin degrades visibly instead of silently
/// stranding the agent.
pub fn adapter_for(plugin: &CliPlugin) -> &'static dyn CliAdapter {
    match plugin.mcp_format {
        McpFormat::ClaudeLocalScope => &claude::ClaudeAdapter,
        McpFormat::CodexGlobalToml => &codex::CodexAdapter,
        McpFormat::OpencodeJson => &opencode::OpencodeAdapter,
        McpFormat::ReasonixMcpJson => &reasonix::ReasonixAdapter,
        McpFormat::ZuluMcpJson => &zulu::ZuluAdapter,
        McpFormat::KimiMcpJson => &kimi::KimiAdapter,
        // No MCP format declared: still route by trust format so a trust-only
        // CLI lands on the right family; otherwise the generic floor.
        McpFormat::None => match plugin.trust_format {
            TrustFormat::ClaudeJson => &claude::ClaudeAdapter,
            TrustFormat::CodexToml => &codex::CodexAdapter,
            TrustFormat::None => &GenericAdapter,
        },
    }
}

/// Fallback for a plugin whose formats match no known family. It performs no
/// patches; it only WARNS when an `auto_*` flag is set but there's no adapter to
/// honor it, so the failure is visible (mirrors the old `run_patches` "format =
/// none" warnings).
struct GenericAdapter;

impl CliAdapter for GenericAdapter {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn pre_spawn(&self, plugin: &CliPlugin, _workspace: &Path, _ctx: &PreSpawnCtx) {
        if plugin.auto_inject_mcp {
            tracing::warn!(
                cli = %plugin.id,
                "no adapter matched this CLI's formats; agent will have NO swarm_* tools (cannot coordinate)"
            );
        }
        if plugin.auto_trust_workspace || plugin.auto_inject_stop_hook || plugin.auto_dismiss_update
        {
            tracing::warn!(
                cli = %plugin.id,
                "no adapter matched this CLI's formats; trust / update / stop-hook patches skipped"
            );
        }
    }
}

/// Probe `<binary> --help` once and cache whether `flag` appears anywhere in
/// stdout or stderr. Used by adapters to feature-detect CLI flags whose absence
/// would crash spawn (codex 0.130 rejects unknown argv with a non-zero exit —
/// adding a future-only flag unconditionally would brick every spawn on the
/// older version).
///
/// Cache key is `(binary, flag)` so different plugins probing different flags
/// don't collide. The cache is process-lifetime — a CLI upgrade requires a
/// server restart to re-probe, which is fine for the local single-user model.
///
/// Errors and timeouts on the probe fall through as `false`: if we can't confirm
/// the flag is supported, we don't inject it.
///
/// The probe is **timeout-bounded** (F17): `<binary> --help` runs on a worker
/// thread and we wait at most `PROBE_TIMEOUT` for it via `recv_timeout`. This fn
/// is called synchronously on the async spawn path, so an unresponsive `--help`
/// must not be able to stall a spawn forever — past the deadline we give up and
/// return `false`. (A genuinely hung `--help` leaves its child + thread
/// lingering until it exits on its own or the server does; a real CLI's `--help`
/// returns in ms, so this is an acceptable bound on a pathological case.)
pub(crate) fn binary_supports_flag(binary: &str, flag: &str) -> bool {
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    static CACHE: OnceLock<Mutex<HashMap<(String, String), bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let key = (binary.to_string(), flag.to_string());
    if let Some(&v) = cache.lock().get(&key) {
        return v;
    }

    let bin = binary.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // output() drains stdout+stderr (so the child can't deadlock on a full
        // pipe) and waits for exit. Result is sent back; ignore send errors
        // (receiver already gave up on timeout).
        let _ = tx.send(
            crate::runtime_path::tool_command(&bin)
                .arg("--help")
                .output(),
        );
    });

    let supported = match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            stdout.contains(flag) || stderr.contains(flag)
        }
        Ok(Err(_)) => false, // spawn / IO error
        Err(_) => {
            // recv timed out — the probe took longer than PROBE_TIMEOUT.
            tracing::warn!(
                binary,
                flag,
                "binary flag probe timed out; assuming unsupported"
            );
            false
        }
    };

    tracing::info!(binary, flag, supported, "binary flag probe result");
    cache.lock().insert(key, supported);
    supported
}
