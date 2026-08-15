//! CLI plugin registry. Each plugin is a `cli-plugins/<id>.toml` describing
//! how to spawn one kind of CLI under our shim. M1 ships claude + codex
//! only; others live in §13 Backlog of the plan.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How this CLI's per-workspace *trust* state is recorded so the headless PTY
/// isn't blocked on a "do you trust this folder?" prompt. Dispatch is keyed on
/// this **format**, not on `plugin.id` — a new CLI that reuses an existing
/// config format needs only the right value here, zero Rust changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrustFormat {
    /// No trust patch (the CLI has no trust gate, or we don't manage it).
    #[default]
    None,
    /// `~/.claude.json projects.<ws>.hasTrustDialogAccepted = true`.
    ClaudeJson,
    /// Appended `[projects."<ws>"] trust_level = "trusted"` in `~/.codex/config.toml`.
    CodexToml,
}

/// How this CLI is told to load the `swarmx-swarm` MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum McpFormat {
    #[default]
    None,
    /// claude: per-project local scope in `~/.claude.json` + a per-agent
    /// `--mcp-config <file> --strict-mcp-config` (injected in spawn.rs) to
    /// dodge the shared-cwd `mcpServers` collision (M6b).
    ClaudeLocalScope,
    /// codex: MCP injection is PER-AGENT today — pre-spawn writes an isolated
    /// `~/.swarmx/codex-home/<agent_id>/config.toml` (the user's `~/.codex`
    /// MCP servers inherited + this agent's own `[mcp_servers.swarmx-swarm]`)
    /// and spawn points the child at it via `CODEX_HOME`; the user's global
    /// `~/.codex/config.toml` is NOT mutated (stale swarm sections from older
    /// swarmx versions are self-healed away). The variant name is historical
    /// (the first implementation DID append one global section): renaming it
    /// would also touch the `cli/mod.rs` dispatch and the CI invariant in
    /// `scripts/harness-check.mjs` that greps for this literal, so the name
    /// (and the `"codex-global-toml"` wire value) stays.
    CodexGlobalToml,
    /// opencode: a per-agent config file at `~/.swarmx/opencode/<agent_id>.json`
    /// carrying `mcp.swarmx-swarm` (local stdio, per-agent identity in
    /// `environment`) + `permission = "allow"` + `autoupdate = false`. spawn.rs
    /// points opencode at it via `OPENCODE_CONFIG=<file>`, which (verified live)
    /// DEEP-MERGES on top of the user's config — swarmx's keys win on conflict,
    /// and the user's `provider`/model config is preserved so the worker can run
    /// a model. Per-agent identity is collision-free even in Shared layouts (each
    /// process has its own OPENCODE_CONFIG file; swarmx writes no project-local
    /// opencode.json to clobber). The wake plugin is merged in by `OpencodePlugin`.
    OpencodeJson,
    /// reasonix: a project-root `<ws>/.mcp.json` carrying `mcpServers.swarmx-swarm`
    /// (the Claude Code MCP schema, which reasonix reads as-is — verified live).
    /// Per-agent identity rides in the entry's `args`/`env`. No `--mcp-config`
    /// flag needed: reasonix auto-discovers `.mcp.json` in the session cwd (the
    /// per-agent workspace). Written by `cli::reasonix`.
    ReasonixMcpJson,
    /// zulu (Comate): `<ws>/.comate/mcp.json` carrying `mcpServers.swarmx-swarm`
    /// (standard schema). zulu does NOT read a root `.mcp.json`; its kernel reads
    /// `.comate/mcp.json` (`MCP_SETTINGS_DIR=".comate"` — verified via
    /// `zulu inspect`). Per-agent identity in `args`/`env`. Written by
    /// `cli::zulu`.
    ZuluMcpJson,
    /// kimi (Kimi Code): `<ws>/.kimi-code/mcp.json` carrying
    /// `mcpServers.swarmx-swarm` (standard schema — kimi deep-merges the
    /// project-level file over the user-level `~/.kimi-code/mcp.json`, so the
    /// user's own servers survive). Per-agent identity in `args`/`env`.
    /// Written by `cli::kimi`.
    KimiMcpJson,
}

/// Where/how the wake Stop-hook is materialized (the timeout-unit divergence —
/// claude ms vs codex s — lives inside the per-format writer for now; a future
/// step lifts it into a `[stop_hook]` table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StopHookFormat {
    #[default]
    None,
    /// `<ws>/.claude/settings.local.json` (`hooks.Stop[]`, timeout in ms).
    ClaudeSettingsLocal,
    /// `<ws>/.codex/hooks.json` (`hooks.Stop[]`, timeout in seconds).
    CodexHooksJson,
    /// opencode has NO blocking Stop hook, so "wake" is delivered as an opencode
    /// PLUGIN instead: the swarmx wake plugin (`cli-plugins/opencode/
    /// swarmx-wake.js`) is merged into `plugin[]` of the SAME per-agent config
    /// file `OpencodeJson` writes. On `session.idle` the plugin calls swarmx's
    /// `consume_wakes` endpoint and re-prompts the session when wakes are
    /// pending — the opencode equivalent of the claude/codex Stop-hook wake.
    OpencodePlugin,
    /// kimi (Kimi Code): kimi has NO project-level hooks, so `cli::kimi`
    /// patches ONE managed `[[hooks]] event = "Stop"` entry into the
    /// USER-LEVEL `config.toml` (`$KIMI_CODE_HOME`, else `~/.kimi-code`),
    /// running `swarmx-mcp wake-check --hook-format kimi` (timeout in
    /// SECONDS, kimi's native unit). It fires for every kimi session on the
    /// machine but no-ops for non-swarmx ones (wake-check resolves
    /// `SWARMX_AGENT_ID`, which only swarmx spawns carry). Idempotent and
    /// self-healing on mcp-bin path drift — see `cli::kimi`.
    KimiConfigToml,
}

/// How swarmx delivers a turn's prompt text (the first-turn bootstrap and
/// each wake "kick") into this CLI. Every CLI runs over a PTY; this only picks
/// the INPUT channel:
///
/// - `keystroke` (default) — type the prompt into the CLI's TUI as PTY bytes
///   (bracketed paste + Enter). Works for claude/codex.
/// - `opencode-tui-http` — POST the prompt to opencode's built-in `/tui/*`
///   control API on the agent's `--port`. opencode's TUI can't reliably accept
///   a large (~24k-char) bootstrap via keystrokes — the paste parks without
///   submitting — so we use its own documented HTTP control surface instead.
///   spawn allocates a per-agent port and passes `--port`; the port is stored
///   on the `AgentSlot` and the bootstrap/wake paths deliver via
///   `crate::opencode_tui`. See `crate::opencode_tui`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InputDelivery {
    #[default]
    Keystroke,
    OpencodeTuiHttp,
    /// reasonix: driven over its `reasonix serve` HTTP+SSE control API. spawn
    /// allocates a per-agent port and passes `--addr 127.0.0.1:<port>`; the
    /// bootstrap/wake paths POST to `/submit` and the agent's turns/activity are
    /// followed on the `/events` SSE stream. See `crate::reasonix_serve`.
    ReasonixServeHttp,
    /// zulu (Comate): driven over its `zulu serve` HTTP+SSE control API. spawn
    /// allocates a per-agent port and passes `--host 127.0.0.1 --port <port>`;
    /// the bootstrap/wake paths POST to `/session` (or
    /// `/api/v1/conversations/:id/messages`) and turns/activity/usage are
    /// followed on the SSE response stream (`conversation-status` Completed =
    /// turn-end). Model rides in the POST body, not a spawn arg. See
    /// `crate::zulu_serve`.
    ZuluServeHttp,
}

/// Which account / quota surface a CLI plugin consumes by default. This is a
/// guardrail, not a billing meter: swarmx still cannot see subscription spend
/// from PTY CLIs. The value exists so a future structured transport cannot
/// silently move a user from their interactive subscription into SDK/API
/// credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BillingSurface {
    /// Unknown or unmanaged surface. Safe default for third-party plugins that
    /// have not declared their billing behavior yet.
    #[default]
    Unknown,
    /// Interactive subscription login, e.g. launching `claude` in a PTY.
    InteractiveSubscription,
    /// CLI account surface, e.g. Codex CLI using the user's logged-in CLI state.
    CliAccount,
    /// Non-interactive agent SDK / print-mode credit bucket.
    AgentSdkCredits,
    /// Direct provider API key billing.
    ApiKey,
    /// SaaS license billing, e.g. Comate Zulu's `-l <license>` against
    /// comate.baidu.com. Like ApiKey it's an explicit credential (not a reused
    /// interactive subscription), but a distinct surface for clarity.
    License,
}

/// Kind of a [`ReadyStep`]. `ready_plan` is a **sequential** golutra-style DSL:
/// the host advances through the steps in declared order as PTY output arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReadyStepKind {
    /// Watch PTY output for `needle`; inject `response` once when it appears,
    /// then advance. (A `wait_for` + `input` fused into one step.)
    #[default]
    AnswerDialog,
    /// Block the plan until `needle` appears in PTY output, then advance. Used
    /// to gate later steps on a prompt/banner (replaces a fixed sleep).
    WaitFor,
    /// Inject `response` into the PTY as soon as this step becomes active
    /// (no needle). Used to type an initial command after a preceding wait.
    Input,
    /// When `needle` appears, capture the whitespace-delimited token that
    /// follows it on the same line into the capture slot named by `into`
    /// (golutra resume support), then advance. The captured value is exposed
    /// via `ReadyPlanRunner::captured()` — a future resume path can read it.
    ExtractSessionId,
}

/// One step of a CLI's post-spawn readiness automation — host-side handling
/// of first-spawn TUI dialogs that would otherwise block a headless PTY. This
/// is the **data-driven replacement** for the hard-coded `spawn::DialogAutoAnswer`:
/// any CLI lists its own dialogs in `cli-plugins/<id>.toml`, no Rust change.
///
/// ```toml
/// [[ready_plan]]
/// kind = "answer_dialog"
/// needle = "Hooks need review"
/// response = "2\r"        # TOML honors \r (Enter); injected verbatim
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyStep {
    #[serde(default)]
    pub kind: ReadyStepKind,
    /// Substring to match in PTY output (raw bytes, stitched across chunk
    /// boundaries). Used by `answer_dialog` / `wait_for` / `extract_session_id`.
    #[serde(default)]
    pub needle: String,
    /// Bytes to inject — the dialog answer (`answer_dialog`) or the text to type
    /// (`input`). TOML escapes like `\r` are honored.
    #[serde(default)]
    pub response: String,
    /// Give up on a needle-matching step (`answer_dialog`/`wait_for`/
    /// `extract_session_id`) after this many ms so we don't watch forever; the
    /// plan then advances past it. Default 30s.
    #[serde(default = "default_answer_window_ms")]
    pub window_ms: u64,
    /// `extract_session_id`: name of the capture slot to store the token that
    /// follows `needle` into (read back via `ReadyPlanRunner::captured()`).
    #[serde(default)]
    pub into: String,
}

fn default_answer_window_ms() -> u64 {
    30_000
}

/// A PTY-output signature that means the spawned CLI *cannot actually do work*
/// even though its process is alive — the canonical example is claude printing
/// `Not logged in · Run /login` when no OAuth credential is present, or a
/// rate-limit / quota banner.
///
/// Unlike [`ReadyStep`] (a one-shot ordered cursor for auto-answering startup
/// dialogs), health needles are scanned **continuously and all at once**: the
/// pump's `HealthScanner` watches every needle on every chunk and latches on
/// the first match, raising `LifecycleEvent::HealthFail`. That lets the UI
/// replace the fake "online" green dot + "暂无消息" with an honest, actionable
/// failure card within seconds.
///
/// Data-driven (declared per CLI in `cli-plugins/<id>.toml`) so a new CLI adds
/// its own auth/quota banners with zero Rust changes:
/// ```toml
/// [[health_needles]]
/// needle = "Not logged in"   # literal substring, stitched across PTY chunks
/// reason = "Claude Code 未登录"
/// kind   = "auth"             # auth | rate_limit | fatal (steers remedy buttons)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthNeedle {
    /// Literal substring matched in PTY output (raw bytes, stitched across
    /// chunk boundaries — same matching as `ready_plan` needles).
    pub needle: String,
    /// Short human-facing reason surfaced in the failure card.
    pub reason: String,
    /// Coarse class steering which remedy buttons the UI offers:
    /// `auth` → 打开终端登录, `rate_limit` → 稍后重试, `fatal` → 换引擎.
    /// Open string; unknown kinds fall back to the generic remedy set.
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliPlugin {
    pub id: String,
    pub display_name: String,
    pub binary: String,
    #[serde(default)]
    pub default_args: Vec<String>,
    /// Env var name to pass through from the server process (so the CLI
    /// can find its OAuth credentials at `$HOME/.claude/` etc.). Default
    /// is "HOME".
    #[serde(default = "default_home_env")]
    pub home_env: String,
    /// Billing/quota surface this plugin is expected to use by default.
    #[serde(default)]
    pub billing_surface: BillingSurface,
    /// If true, spawning this plugin requires an explicit opt-in env var when
    /// its billing surface is not the normal interactive/CLI-account path.
    #[serde(default)]
    pub requires_explicit_billing_opt_in: bool,
    /// Env var prefixes to strip from the child process even if the provider
    /// allowlist would otherwise forward them. Claude blocks `ANTHROPIC_` by
    /// default so an ambient API key cannot switch it away from subscription
    /// billing.
    #[serde(default)]
    pub blocked_env_prefixes: Vec<String>,
    /// Provider env prefixes THIS CLI authenticates with, declared explicitly
    /// per plugin (e.g. reasonix declares `DEEPSEEK_`, codex `OPENAI_`).
    /// `spawn.rs` forwards ONLY these prefixes into the child, still
    /// intersected with `blocked_env_prefixes` above (a blocked prefix never
    /// crosses — claude declares `ANTHROPIC_` but blocks it by default, the
    /// billing red line). This replaced a hard-coded
    /// `ANTHROPIC_/OPENAI_/CLAUDE_/DEEPSEEK_` list that forwarded EVERY
    /// provider's keys into EVERY plugin: these CLIs are full-shell agents,
    /// so an ambient foreign key (say `ANTHROPIC_API_KEY` inside codex) is
    /// plaintext handed to the model. Empty = no provider env at all (zulu
    /// passes an explicit license argv; claude/kimi ride OAuth from `$HOME`).
    #[serde(default)]
    pub provider_env_prefixes: Vec<String>,
    /// If true, the host patches the CLI's per-workspace trust state before
    /// spawn so the CLI doesn't prompt "Do you trust this folder?" — fine
    /// for swarmx because workspaces always live under `~/.swarmx/`
    /// (created by us). Currently only honoured for `id = "claude"` (writes
    /// `~/.claude.json: projects[<ws>].hasTrustDialogAccepted = true`).
    #[serde(default)]
    pub auto_trust_workspace: bool,
    /// If true, the host suppresses the CLI's "an update is available"
    /// prompt before spawn — those prompts otherwise block the headless PTY
    /// waiting on a single keystroke we have no way to deliver. Currently
    /// only honoured for `id = "codex"` (writes
    /// `~/.codex/version.json: dismissed_version = latest_version`).
    #[serde(default)]
    pub auto_dismiss_update: bool,
    /// If true, the host writes (or refreshes) an MCP server entry pointing
    /// at the `swarmx-mcp` binary so the spawned agent can call swarm
    /// tools (send_message / blackboard / …) from inside its native toolbox.
    /// Dispatch is keyed on `mcp_format`, NOT on `id` (see [`McpFormat`] for
    /// the full per-format detail); the shipped manifests use:
    ///   - claude   — `~/.claude.json projects.<ws>.mcpServers.swarmx-swarm`
    ///     (local scope, no approval prompt) + a per-agent `--mcp-config`
    ///   - codex    — a PER-AGENT `~/.swarmx/codex-home/<agent_id>/config.toml`
    ///     injected via `CODEX_HOME` (the user's global `~/.codex/config.toml`
    ///     is no longer mutated; see [`McpFormat::CodexGlobalToml`])
    ///   - opencode — a per-agent `~/.swarmx/opencode/<agent_id>.json` via
    ///     `OPENCODE_CONFIG` (deep-merges over the user's config)
    ///   - reasonix / zulu / kimi — a project-level
    ///     `<ws>/.mcp.json` / `<ws>/.comate/mcp.json` / `<ws>/.kimi-code/mcp.json`
    #[serde(default)]
    pub auto_inject_mcp: bool,
    /// If true, the host installs a workspace-local Stop hook that runs
    /// `swarmx-mcp wake-check` at every turn boundary, giving the agent
    /// a synthetic continuation prompt whenever its swarm inbox has unread
    /// messages. Currently honoured for:
    ///   - `id = "claude"` — writes `<workspace>/.claude/settings.local.json`
    ///     `hooks.Stop[]` (timeout in milliseconds).
    ///   - `id = "codex"`  — writes `<workspace>/.codex/hooks.json`
    ///     `hooks.Stop[]` (timeout in seconds).
    ///
    /// Merge-not-clobber: existing user hooks are preserved; swarmx's
    /// entry is appended once (idempotent on re-spawn).
    #[serde(default)]
    pub auto_inject_stop_hook: bool,
    /// Post-spawn readiness automation: auto-answer first-spawn TUI dialogs
    /// that would block a headless PTY (e.g. codex 0.130+'s "Hooks need
    /// review" menu). Data-driven — replaces the old `auto_answer_hooks_dialog`
    /// bool + the hard-coded needle/response in Rust. See [`ReadyStep`] and
    /// `spawn::ReadyPlanRunner`. Empty for CLIs with no blocking dialogs.
    #[serde(default)]
    pub ready_plan: Vec<ReadyStep>,

    /// PTY-output signatures that mean "alive but can't work" (not logged in,
    /// rate limited, invalid key). Scanned **continuously** by the pump's
    /// `HealthScanner` (not the one-shot `ready_plan` cursor); the first match
    /// raises `LifecycleEvent::HealthFail` → `AgentState::Error`. Empty = no
    /// health probing for this CLI. See [`HealthNeedle`].
    #[serde(default)]
    pub health_needles: Vec<HealthNeedle>,

    /// Which trust-config format to write when `auto_trust_workspace` is set.
    /// Dispatch is keyed on this, NOT on `plugin.id` (see `TrustFormat`).
    #[serde(default)]
    pub trust_format: TrustFormat,
    /// Which MCP-injection format to use when `auto_inject_mcp` is set.
    #[serde(default)]
    pub mcp_format: McpFormat,
    /// Which Stop-hook format to install when `auto_inject_stop_hook` is set.
    #[serde(default)]
    pub stop_hook_format: StopHookFormat,
    /// Stop-hook `timeout` value, written verbatim into the hook config in the
    /// CLI's NATIVE unit (claude/settings.local.json = milliseconds, codex/
    /// hooks.json = seconds). Externalized from a Rust constant so a new CLI
    /// declares its own value+unit instead of inheriting a 1000×-wrong one.
    #[serde(default = "default_stop_hook_timeout")]
    pub stop_hook_timeout: i64,
    /// Frontend keystroke-settle delay (ms) applied after the CLI signals
    /// ready, for CLIs whose TUI swallows the first byte if sent too early
    /// (codex's ratatui input poll attaches ~120-180ms after OSC_READY).
    /// Surfaced in `CliPluginInfo` so the web terminal's input policy is
    /// data-driven instead of branching on the agent-id prefix. 0 = none.
    #[serde(default)]
    pub input_settle_ms: u64,

    /// L5c — model overlay. Argv template for passing a model to this CLI, with
    /// a `{model}` placeholder substituted at spawn time. claude & codex both
    /// take `["--model", "{model}"]`. Lives in the manifest (not Rust) so the
    /// "host ≠ model" axiom holds: the same CLI runs any model without forking
    /// the plugin id or a role. Empty ⇒ this CLI can't take a model override.
    #[serde(default)]
    pub model_args: Vec<String>,
    /// Default model applied when a spawn doesn't pass one. `None` ⇒ let the CLI
    /// pick its own default. A per-spawn `model` (REST/MCP) overrides this.
    #[serde(default)]
    pub default_model: Option<String>,
    /// True iff this CLI's `--model` natively accepts the abstract tier names
    /// (opus/sonnet/haiku) as aliases — i.e. the tier vocabulary IS this CLI's
    /// own model lineup. Only claude. For others (codex = gpt-5.x) those names
    /// are meaningless, so the 模型 settings page must NOT present opus/sonnet/
    /// haiku rows for them — it shows just their real default model + effort.
    #[serde(default)]
    pub native_tiers: bool,

    /// Reasoning/thinking effort overlay (parallel to `model_args`). Argv
    /// template with an `{effort}` placeholder substituted at spawn — claude:
    /// `["--effort", "{effort}"]`, codex: `["-c", "model_reasoning_effort={effort}"]`.
    /// Empty ⇒ this CLI can't take an effort override. Both 2026 CLIs converged
    /// on discrete effort levels and degrade gracefully if a level outranks the
    /// model, so the same value is safe across models.
    #[serde(default)]
    pub effort_args: Vec<String>,
    /// Maps an ABSTRACT effort level (low|medium|high|max) to this CLI's concrete
    /// value (e.g. claude max→"max", codex max→"xhigh"). An abstract level absent
    /// here (or "default") emits no effort args = the model's own default. Keeps
    /// the host model-agnostic — a new CLI declares its own ladder in TOML.
    #[serde(default)]
    pub effort_levels: HashMap<String, String>,

    /// How a turn's prompt (bootstrap + wakes) is delivered into this CLI.
    /// Defaults to `keystroke` (type into the PTY's TUI). opencode declares
    /// `opencode-tui-http` because its TUI can't take a large bootstrap via
    /// keystrokes — see [`InputDelivery`] and `crate::opencode_tui`.
    #[serde(default)]
    pub input_delivery: InputDelivery,

    /// Keystroke-delivery framing: wrap the bootstrap paste in explicit
    /// bracketed-paste markers (`ESC[200~` … `ESC[201~`) instead of relying on
    /// the TUI's burst heuristic. kimi's TUI intermittently wedges on a large
    /// (~26KB) raw burst — the paste renders but the trailing `\r` is absorbed
    /// as a newline and the turn never starts (context stays 0%; live-verified
    /// twice, incl. a standalone PTY probe where the raw burst wedged the TUI
    /// hard enough to resist Ctrl-C). The explicit markers make the paste
    /// atomic: the closing `ESC[201~` ends it deterministically and the next
    /// `\r` submits. Default false — claude/codex are proven on the raw-burst
    /// path and don't need it.
    #[serde(default)]
    pub bracketed_paste: bool,

    /// PTY-output needle that must appear BEFORE the keystroke bootstrap is
    /// pasted. kimi fetches the MCP tool list EARLY (firing swarmx's
    /// mcp-ready ping) but keeps initializing for another ~0.1-0.9s — a paste
    /// landing before its "MCP server … connected" banner is silently eaten
    /// when the input pipeline resets (ctx stays 0%; measured 2/4 spawns).
    /// Empty = no gate (claude/codex paste right after mcp-ready and are
    /// proven). The wait is bounded in the inject path — a missing needle
    /// injects anyway and the first-response watchdog judges the outcome.
    #[serde(default)]
    pub bootstrap_ready_needle: String,
    /// Extra settle (ms) AFTER the readiness needle appears, before the paste
    /// — absorbs the TUI's final input-pipeline reset (kimi's banner→settled
    /// gap measured at ~0.1-0.9s; 1000ms matches the observed good case).
    #[serde(default)]
    pub bootstrap_ready_settle_ms: u64,
}

fn default_home_env() -> String {
    "HOME".into()
}
fn default_stop_hook_timeout() -> i64 {
    10_000
}

#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    plugins: HashMap<String, CliPlugin>,
}

impl PluginRegistry {
    /// Load every `cli-plugins/*.toml` from a single dir. **One malformed file
    /// is skipped with a warning, not fatal** — a single bad/partial plugin
    /// must not take down ALL CLIs (and thus server boot). This mirrors
    /// `roles::RoleRegistry`'s tolerant policy; a missing dir is also non-fatal
    /// (empty registry). Returns `Result` for call-site compatibility, but is
    /// effectively infallible (every error path warns + skips).
    pub fn load_dir(dir: &Path) -> Result<Self> {
        let mut plugins: HashMap<String, CliPlugin> = HashMap::new();
        let mut source: HashMap<String, PathBuf> = HashMap::new();
        Self::merge_dir(dir, &mut plugins, &mut source);
        Ok(Self { plugins })
    }

    /// Load plugins from `dirs` in order; an id present in a LATER dir
    /// overrides the same id from an earlier one (last-writer-wins). This is
    /// how a user customizes or adds a CLI without forking the repo (L5a):
    /// drop a `~/.swarmx/cli-plugins/<id>.toml` and it shadows the bundled
    /// definition. Per-layer resilience is preserved — a bad file or missing
    /// layer is warn-skipped, never fatal.
    pub fn load_layered(dirs: &[PathBuf]) -> Self {
        // Seed from the compiled-in builtin catalog FIRST, so a packaged app
        // (whose CWD has no `cli-plugins/` dir, and whose CARGO_MANIFEST_DIR
        // points at a build-machine path that doesn't exist on the user's box)
        // still has claude/codex and can spawn agents. On-disk layers then
        // OVERLAY by id (last-writer-wins), so dev edits in the repo
        // `cli-plugins/` dir and a user's `~/.swarmx/cli-plugins/` still take
        // effect. Mirrors `roles::RoleRegistry::builtin()` + dir overlay — the
        // proven pattern; without this base layer the registry was EMPTY on a
        // user machine and EVERY spawn failed NOT_FOUND.
        let mut reg = Self::builtin();
        let mut source: HashMap<String, PathBuf> = HashMap::new();
        for dir in dirs {
            Self::merge_dir(dir, &mut reg.plugins, &mut source);
        }
        if reg.plugins.is_empty() {
            tracing::warn!(
                layers = dirs.len(),
                "no CLI plugins loaded from builtins or any layer; spawns will fail with NOT_FOUND"
            );
        }
        reg
    }

    /// Compiled-in CLI plugin catalog — `claude` and `codex` baked into the
    /// binary via `include_str!` so a deployed server with no reachable
    /// `cli-plugins/` dir still spawns agents (the plugins carry
    /// binary/args/trust/mcp/stop-hook/health config). A malformed embed
    /// `warn!` + skips — a parse slip never aborts startup. Mirrors
    /// `roles::RoleRegistry::builtin()`; the on-disk layers in `load_layered`
    /// overlay this base.
    pub fn builtin() -> Self {
        const BUILTIN: &[(&str, &str)] = &[
            (
                "claude.toml",
                include_str!("../../../cli-plugins/claude.toml"),
            ),
            ("codex.toml", include_str!("../../../cli-plugins/codex.toml")),
            (
                "opencode.toml",
                include_str!("../../../cli-plugins/opencode.toml"),
            ),
            (
                "reasonix.toml",
                include_str!("../../../cli-plugins/reasonix.toml"),
            ),
            ("zulu.toml", include_str!("../../../cli-plugins/zulu.toml")),
            ("kimi.toml", include_str!("../../../cli-plugins/kimi.toml")),
        ];
        let mut plugins: HashMap<String, CliPlugin> = HashMap::new();
        for (name, content) in BUILTIN {
            match toml::from_str::<CliPlugin>(content) {
                Ok(p) => {
                    plugins.insert(p.id.clone(), p);
                }
                Err(err) => {
                    tracing::warn!(?err, plugin = name, "skip builtin cli-plugin: parse failed");
                }
            }
        }
        Self { plugins }
    }

    /// Merge one dir's `*.toml` into `plugins`, recording each id's source path
    /// so an override (same id seen again, from this or a later layer) logs the
    /// shadowing. All IO/parse errors warn + skip; an absent dir is silent
    /// (the user override layer is normally absent).
    fn merge_dir(
        dir: &Path,
        plugins: &mut HashMap<String, CliPlugin>,
        source: &mut HashMap<String, PathBuf>,
    ) {
        if !dir.is_dir() {
            tracing::debug!(dir = %dir.display(), "cli-plugins layer absent; skipping");
            return;
        }
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(dir = %dir.display(), ?err, "cli-plugins layer unreadable; skipping");
                return;
            }
        };
        for entry in read {
            let path = match entry {
                Ok(e) => e.path(),
                Err(err) => {
                    tracing::warn!(?err, "skip unreadable cli-plugins dir entry");
                    continue;
                }
            };
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let bytes = match std::fs::read_to_string(&path) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(path = %path.display(), ?err, "skip cli-plugin: read failed");
                    continue;
                }
            };
            let plugin: CliPlugin = match toml::from_str(&bytes) {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "skip cli-plugin: parse failed");
                    continue;
                }
            };
            let id = plugin.id.clone();
            if let Some(prev) = source.get(&id) {
                tracing::info!(
                    id = %id,
                    shadowed = %prev.display(),
                    by = %path.display(),
                    "cli-plugin overridden by a later layer"
                );
            }
            source.insert(id.clone(), path.clone());
            plugins.insert(id, plugin);
        }
    }

    pub fn get(&self, id: &str) -> Option<&CliPlugin> {
        self.plugins.get(id)
    }

    pub fn list(&self) -> Vec<&CliPlugin> {
        let mut v: Vec<_> = self.plugins.values().collect();
        v.sort_by_key(|p| p.id.clone());
        v
    }
}


/// Locate the `cli-plugins/` directory: first the path from env
/// `SWARMX_CLI_PLUGINS_DIR`, otherwise `<workspace>/cli-plugins` relative
/// to the binary's manifest dir (during dev) or CWD.
pub fn default_plugins_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SWARMX_CLI_PLUGINS_DIR") {
        return PathBuf::from(p);
    }
    // CARGO_MANIFEST_DIR resolves to `crates/swarmx-server` at build
    // time; step up two levels to reach the workspace root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(ws) = manifest.parent().and_then(|p| p.parent()) {
        let candidate = ws.join("cli-plugins");
        if candidate.is_dir() {
            return candidate;
        }
    }
    // Last-resort bare-relative fallback. In the packaged app CWD is `/`, so this
    // resolves to `/cli-plugins` — which does not exist — and `load_layered`
    // overlays NOTHING onto the compiled-in builtins. Tolerable HERE only because
    // claude/codex are embedded (`PluginRegistry::builtin()`), but a silent miss
    // masks a broken install (env/manifest both failed to resolve). Warn ONCE so
    // release self-checks catch it instead of debugging it live.
    static WARN_ONCE: std::sync::Once = std::sync::Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            "SWARMX_CLI_PLUGINS_DIR unset and CARGO_MANIFEST_DIR-relative `cli-plugins/` not \
             found; falling back to CWD-relative `cli-plugins` — unreliable under the installed \
             app (CWD=/). Set SWARMX_CLI_PLUGINS_DIR (Tauri sidecar) or rely on the embedded \
             builtin plugins."
        );
    });
    PathBuf::from("cli-plugins")
}

/// User override layer (L5a): `~/.swarmx/cli-plugins/`, or the path in
/// `SWARMX_USER_CLI_PLUGINS_DIR`. Returns `None` when neither resolves, so
/// the caller just omits the layer. The dir need not exist — `load_layered`
/// treats an absent layer as empty. Letting users drop a `<id>.toml` here means
/// adding/overriding a CLI without forking the repo.
pub fn user_plugins_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SWARMX_USER_CLI_PLUGINS_DIR") {
        return Some(PathBuf::from(p));
    }
    crate::runtime_path::swarmx_home().map(|h| h.join(".swarmx").join("cli-plugins"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped manifests must declare the format enums pre_spawn.rs / spawn.rs
    /// dispatch on. Guards the L2 backfill against an accidental drop/typo that
    /// would silently turn an agent into a coordination-dead island.
    #[test]
    fn shipped_manifests_declare_formats() {
        let reg = PluginRegistry::load_dir(&default_plugins_dir()).expect("load shipped plugins");

        let claude = reg.get("claude").expect("claude plugin present");
        assert_eq!(claude.trust_format, TrustFormat::ClaudeJson);
        assert_eq!(claude.mcp_format, McpFormat::ClaudeLocalScope);
        assert_eq!(claude.stop_hook_format, StopHookFormat::ClaudeSettingsLocal);

        let codex = reg.get("codex").expect("codex plugin present");
        assert_eq!(codex.trust_format, TrustFormat::CodexToml);
        assert_eq!(codex.mcp_format, McpFormat::CodexGlobalToml);
        assert_eq!(codex.stop_hook_format, StopHookFormat::CodexHooksJson);

        // opencode (PTY): per-agent opencode.json for MCP+permission, no trust
        // gate, runs as a full-screen TUI in the PTY like claude/codex. Its
        // bootstrap/wakes are delivered over opencode's `/tui/*` HTTP control API
        // (input_delivery = opencode-tui-http) because keystroke paste can't carry
        // a large bootstrap into its TUI; see crate::opencode_tui.
        let opencode = reg.get("opencode").expect("opencode plugin present");
        assert_eq!(opencode.trust_format, TrustFormat::None);
        assert_eq!(opencode.mcp_format, McpFormat::OpencodeJson);
        assert_eq!(opencode.stop_hook_format, StopHookFormat::OpencodePlugin);
        assert_eq!(opencode.input_delivery, InputDelivery::OpencodeTuiHttp);
        assert!(opencode.auto_inject_mcp, "opencode injects swarm MCP");
        assert!(
            opencode.auto_inject_stop_hook,
            "opencode injects the wake plugin"
        );
        assert!(
            !opencode.native_tiers,
            "opencode is provider/model, not opus/sonnet/haiku tiers"
        );
        assert_eq!(opencode.model_args, vec!["--model", "{model}"]);

        // reasonix (HTTP/SSE): driven over `reasonix serve`. MCP via project
        // .mcp.json (Claude Code schema), no trust gate, NO Stop hook (its Stop
        // is observe-only; turn_done on the SSE stream is the turn-end signal),
        // prompts delivered over the serve HTTP control API.
        let reasonix = reg.get("reasonix").expect("reasonix plugin present");
        assert_eq!(reasonix.trust_format, TrustFormat::None);
        assert_eq!(reasonix.mcp_format, McpFormat::ReasonixMcpJson);
        assert_eq!(reasonix.stop_hook_format, StopHookFormat::None);
        assert_eq!(reasonix.input_delivery, InputDelivery::ReasonixServeHttp);
        assert!(reasonix.auto_inject_mcp, "reasonix injects swarm MCP");
        assert!(
            !reasonix.auto_inject_stop_hook,
            "reasonix uses serve SSE turn_done, not a Stop hook"
        );
        assert!(
            !reasonix.native_tiers,
            "reasonix is DeepSeek provider/model, not opus/sonnet/haiku tiers"
        );
        assert_eq!(reasonix.model_args, vec!["--model", "{model}"]);
        assert_eq!(reasonix.billing_surface, BillingSurface::ApiKey);
        assert_eq!(
            reasonix.default_model.as_deref(),
            Some("deepseek-flash"),
            "reasonix defaults to the cheap deepseek-flash provider"
        );
        assert!(
            reasonix.blocked_env_prefixes.is_empty(),
            "reasonix must NOT block DEEPSEEK_ (it needs the key)"
        );

        // zulu (Comate, HTTP/SSE): driven over `zulu serve`. MCP written to
        // `.comate/mcp.json` (zulu's kernel reads that); no trust gate, NO Stop
        // hook, license billing. Model is per-request so model_args is EMPTY.
        let zulu = reg.get("zulu").expect("zulu plugin present");
        assert_eq!(zulu.trust_format, TrustFormat::None);
        assert_eq!(zulu.mcp_format, McpFormat::ZuluMcpJson);
        assert_eq!(zulu.stop_hook_format, StopHookFormat::None);
        assert_eq!(zulu.input_delivery, InputDelivery::ZuluServeHttp);
        assert_eq!(zulu.billing_surface, BillingSurface::License);
        assert!(
            zulu.auto_inject_mcp,
            "zulu injects swarm MCP via .comate/mcp.json"
        );
        assert!(
            !zulu.auto_inject_stop_hook,
            "zulu uses serve SSE Completed status, not a Stop hook"
        );
        assert!(
            !zulu.native_tiers,
            "zulu's 14 models aren't opus/sonnet/haiku tiers"
        );
        assert!(
            zulu.model_args.is_empty(),
            "zulu model is per-request (POST body), not a spawn arg"
        );
        assert_eq!(zulu.default_args, vec!["serve"]);
        assert!(
            zulu.blocked_env_prefixes.is_empty(),
            "zulu passes the license explicitly; nothing ambient to block"
        );

        // kimi (Kimi Code, interactive PTY like claude/codex): MCP via
        // project-level `.kimi-code/mcp.json`; no trust gate; Stop hook is ONE
        // managed entry in the USER-LEVEL config.toml (kimi has no
        // project-level hooks), timeout in SECONDS; keystroke delivery;
        // OAuth subscription billing with the ambient KIMI_MODEL_* API-key
        // override family blocked.
        let kimi = reg.get("kimi").expect("kimi plugin present");
        assert_eq!(kimi.trust_format, TrustFormat::None);
        assert_eq!(kimi.mcp_format, McpFormat::KimiMcpJson);
        assert_eq!(kimi.stop_hook_format, StopHookFormat::KimiConfigToml);
        assert_eq!(kimi.input_delivery, InputDelivery::Keystroke);
        assert_eq!(kimi.billing_surface, BillingSurface::InteractiveSubscription);
        assert!(
            kimi.auto_inject_mcp,
            "kimi injects swarm MCP via .kimi-code/mcp.json"
        );
        assert!(
            kimi.auto_inject_stop_hook,
            "kimi wake rides a config.toml [[hooks]] Stop entry"
        );
        assert!(
            !kimi.auto_trust_workspace,
            "kimi has no workspace trust gate"
        );
        assert!(
            !kimi.native_tiers,
            "kimi's model aliases aren't opus/sonnet/haiku tiers"
        );
        assert_eq!(kimi.model_args, vec!["--model", "{model}"]);
        assert_eq!(kimi.default_args, vec!["--yolo"]);
        assert!(
            kimi.bracketed_paste,
            "kimi's TUI wedges on large raw paste bursts — bootstrap must use explicit bracketed-paste markers",
        );
        assert_eq!(
            kimi.bootstrap_ready_needle,
            "MCP server \"swarmx-swarm\" connected",
            "kimi's bootstrap must gate on its TUI's own MCP-connect banner (early pastes are eaten)",
        );
        assert_eq!(kimi.bootstrap_ready_settle_ms, 1000);
        assert_eq!(
            kimi.stop_hook_timeout, 10,
            "kimi stop-hook timeout in SECONDS (their native unit, range 1-600)"
        );
        assert!(
            kimi.blocked_env_prefixes.iter().any(|p| p == "KIMI_MODEL_"),
            "kimi must block ambient KIMI_MODEL_* (API-key provider override) by default",
        );

        // The codex "Hooks need review" auto-answer migrated from the old
        // auto_answer_hooks_dialog bool into a data-driven ready_plan step.
        assert!(
            codex.ready_plan.iter().any(|s| {
                s.kind == ReadyStepKind::AnswerDialog
                    && s.needle == "Hooks need review"
                    && s.response == "2\r"
            }),
            "codex.toml must carry the Hooks-need-review answer_dialog in ready_plan",
        );
        // claude has no first-spawn blocking dialog → empty plan.
        assert!(claude.ready_plan.is_empty(), "claude ships no ready_plan");

        // Stop-hook timeout is externalized to the manifest in each CLI's
        // native unit (claude = ms, codex = s), not a Rust constant.
        assert_eq!(
            claude.stop_hook_timeout, 10_000,
            "claude stop-hook timeout in ms"
        );
        assert_eq!(
            codex.stop_hook_timeout, 10,
            "codex stop-hook timeout in seconds"
        );

        // Frontend keystroke-settle delay is data-driven from the manifest
        // (surfaced via CliPluginInfo), not a startsWith('codex-') branch.
        assert_eq!(claude.input_settle_ms, 0, "claude needs no settle delay");
        assert_eq!(
            codex.input_settle_ms, 300,
            "codex needs a ~300ms settle delay"
        );

        // L5c: both ship the model-overlay template so a spawn-time model is
        // passed via the manifest, not a hardcoded Rust flag.
        assert_eq!(
            claude.model_args,
            vec!["--model", "{model}"],
            "claude model_args"
        );
        assert_eq!(
            codex.model_args,
            vec!["--model", "{model}"],
            "codex model_args"
        );

        // Billing guardrails: Claude remains on interactive subscription PTY,
        // with ambient ANTHROPIC_* API credentials blocked so it cannot
        // silently switch to API billing.
        assert_eq!(
            claude.billing_surface,
            BillingSurface::InteractiveSubscription,
            "claude must stay on interactive subscription billing by default",
        );
        assert!(
            claude
                .blocked_env_prefixes
                .iter()
                .any(|p| p == "ANTHROPIC_"),
            "claude must block ambient ANTHROPIC_* env by default",
        );
        assert_eq!(codex.billing_surface, BillingSurface::CliAccount);

        // Provider env is declared PER PLUGIN (spawn forwards only these
        // prefixes, intersected with blocked_env_prefixes) — the isolation
        // that keeps one provider's keys out of another full-shell agent.
        // claude declares ANTHROPIC_ only to keep an explicit user opt-in
        // reachable; the block above still wins by default.
        assert_eq!(
            claude.provider_env_prefixes,
            vec!["ANTHROPIC_", "CLAUDE_"],
            "claude declares its own family; ANTHROPIC_ stays blocked by default"
        );
        assert_eq!(codex.provider_env_prefixes, vec!["OPENAI_"]);
        assert!(
            opencode.provider_env_prefixes.is_empty(),
            "opencode authenticates via its own auth.json, not ambient env keys"
        );
        assert_eq!(
            reasonix.provider_env_prefixes,
            vec!["DEEPSEEK_"],
            "reasonix is the ONLY plugin that receives DEEPSEEK_*"
        );
        assert!(
            zulu.provider_env_prefixes.is_empty(),
            "zulu auth is the explicit license argv, never an ambient key"
        );
        assert!(
            kimi.provider_env_prefixes.is_empty(),
            "kimi rides OAuth from ~/.kimi-code; KIMI_MODEL_* stays blocked"
        );

        // Input delivery: claude/codex type into the PTY's TUI (keystroke,
        // the default); opencode is driven over its `/tui/*` HTTP control API.
        assert_eq!(
            claude.input_delivery,
            InputDelivery::Keystroke,
            "claude delivers prompts as keystrokes"
        );
        assert_eq!(
            codex.input_delivery,
            InputDelivery::Keystroke,
            "codex delivers prompts as keystrokes"
        );
    }

    /// The compiled-in catalog must carry claude + codex. Without these baked
    /// into the binary, a packaged app (no reachable cli-plugins/ dir) gets an
    /// EMPTY registry and EVERY agent spawn fails NOT_FOUND — the bug this guards.
    #[test]
    fn builtin_ships_claude_and_codex() {
        let reg = PluginRegistry::builtin();
        assert!(reg.get("claude").is_some(), "claude must be embedded");
        assert!(reg.get("codex").is_some(), "codex must be embedded");
        assert!(reg.get("opencode").is_some(), "opencode must be embedded");
        assert!(reg.get("kimi").is_some(), "kimi must be embedded");
    }

    /// Simulate the user-machine condition: load_layered with only unreachable
    /// dirs must still yield claude/codex from the builtin base layer.
    #[test]
    fn load_layered_falls_back_to_builtins_when_no_dir_resolves() {
        let reg = PluginRegistry::load_layered(&[PathBuf::from("/nonexistent/swarmx-no-such-dir")]);
        assert!(reg.get("claude").is_some(), "claude must survive empty layers");
        assert!(reg.get("codex").is_some(), "codex must survive empty layers");
        assert!(
            reg.get("opencode").is_some(),
            "opencode must survive empty layers"
        );
    }

    /// A new field with a kebab-case typo must FAIL parse (→ warn-skip at load),
    /// never silently deserialize to `None` and strand the agent.
    #[test]
    fn typoed_format_value_fails_parse() {
        let good = r#"id="x"
display_name="X"
binary="x"
mcp_format="claude-local-scope"
"#;
        assert!(toml::from_str::<CliPlugin>(good).is_ok());

        let typo = r#"id="x"
display_name="X"
binary="x"
mcp_format="claude-locl-scope"
"#;
        assert!(
            toml::from_str::<CliPlugin>(typo).is_err(),
            "a typo'd mcp_format must be rejected, not defaulted to None"
        );
    }

    /// Formats default to `None` when omitted, so an unconfigured CLI degrades
    /// loudly (run_patches warns) rather than mis-dispatching.
    #[test]
    fn formats_default_to_none() {
        let minimal = r#"id="x"
display_name="X"
binary="x"
"#;
        let p: CliPlugin = toml::from_str(minimal).unwrap();
        assert_eq!(p.trust_format, TrustFormat::None);
        assert_eq!(p.mcp_format, McpFormat::None);
        assert_eq!(p.stop_hook_format, StopHookFormat::None);
    }

    /// L5a: a user-layer `<id>.toml` overrides the bundled one (last-writer-wins),
    /// a user-only id is added, and a malformed user file is skipped without
    /// dropping the bundled plugins.
    #[test]
    fn layered_override_last_writer_wins() {
        let base = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();

        // Bundled layer: claude + codex.
        std::fs::write(
            base.path().join("claude.toml"),
            "id=\"claude\"\ndisplay_name=\"Claude (bundled)\"\nbinary=\"claude\"\n",
        )
        .unwrap();
        std::fs::write(
            base.path().join("codex.toml"),
            "id=\"codex\"\ndisplay_name=\"Codex\"\nbinary=\"codex\"\n",
        )
        .unwrap();

        // User layer: override claude's binary + add a brand-new gemini.
        std::fs::write(
            user.path().join("claude.toml"),
            "id=\"claude\"\ndisplay_name=\"Claude (user)\"\nbinary=\"/opt/claude-nightly\"\n",
        )
        .unwrap();
        std::fs::write(
            user.path().join("gemini.toml"),
            "id=\"gemini\"\ndisplay_name=\"Gemini\"\nbinary=\"gemini\"\n",
        )
        .unwrap();
        // A malformed user file must not nuke the rest.
        std::fs::write(user.path().join("broken.toml"), "id = \nnot valid toml").unwrap();

        let reg =
            PluginRegistry::load_layered(&[base.path().to_path_buf(), user.path().to_path_buf()]);

        // claude came from the user layer (override won).
        let claude = reg.get("claude").expect("claude present");
        assert_eq!(claude.display_name, "Claude (user)");
        assert_eq!(claude.binary, "/opt/claude-nightly");
        // codex untouched (only in bundled layer).
        assert_eq!(reg.get("codex").unwrap().display_name, "Codex");
        // gemini added purely from the user layer.
        assert!(reg.get("gemini").is_some(), "user-only CLI added");
        // broken.toml skipped, everyone else survived. opencode comes from the
        // builtin floor (load_layered seeds builtins first), so it's present too.
        assert!(reg.get("opencode").is_some(), "opencode from builtin floor");
        assert!(reg.get("reasonix").is_some(), "reasonix from builtin floor");
        assert!(reg.get("zulu").is_some(), "zulu from builtin floor");
        assert!(reg.get("kimi").is_some(), "kimi from builtin floor");
        assert_eq!(
            reg.list().len(),
            7,
            "claude + codex + gemini + opencode(builtin) + reasonix(builtin) + zulu(builtin) + kimi(builtin)"
        );
    }

    /// An entirely absent user layer is a no-op (the common case), and even
    /// ALL-absent layers still yield the compiled-in builtins (claude + codex)
    /// — never empty, never a panic/error. This is the safety net (the bug fix)
    /// that keeps a packaged app spawning agents when no `cli-plugins/` dir is
    /// reachable on the user's machine.
    #[test]
    fn layered_tolerates_absent_layers() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(
            base.path().join("claude.toml"),
            "id=\"claude\"\ndisplay_name=\"Claude (dir)\"\nbinary=\"claude\"\n",
        )
        .unwrap();
        let missing = base.path().join("does-not-exist");

        // base overrides the builtin claude; codex survives from builtins; the
        // missing layer adds nothing.
        let reg = PluginRegistry::load_layered(&[base.path().to_path_buf(), missing.clone()]);
        assert_eq!(
            reg.get("claude").unwrap().display_name,
            "Claude (dir)",
            "dir layer overrides builtin claude"
        );
        assert!(reg.get("codex").is_some(), "codex stays from builtins");
        assert!(reg.get("opencode").is_some(), "opencode stays from builtins");
        assert!(reg.get("reasonix").is_some(), "reasonix stays from builtins");
        assert!(reg.get("zulu").is_some(), "zulu stays from builtins");
        assert!(reg.get("kimi").is_some(), "kimi stays from builtins");
        assert_eq!(
            reg.list().len(),
            6,
            "claude(dir) + codex(builtin) + opencode(builtin) + reasonix(builtin) + zulu(builtin) + kimi(builtin)"
        );

        // All-absent layers → fall back to the builtin catalog, not empty.
        let only_builtins = PluginRegistry::load_layered(&[missing]);
        assert!(only_builtins.get("claude").is_some());
        assert!(only_builtins.get("codex").is_some());
        assert!(only_builtins.get("opencode").is_some());
        assert!(only_builtins.get("reasonix").is_some());
        assert!(only_builtins.get("zulu").is_some());
        assert!(only_builtins.get("kimi").is_some());
        assert_eq!(only_builtins.list().len(), 6, "builtins are the floor");
    }
}
