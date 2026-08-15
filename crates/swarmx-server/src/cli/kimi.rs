//! Kimi Code CLI adapter — writes the swarm MCP config where kimi reads it,
//! and installs the wake Stop-hook into the USER-LEVEL kimi `config.toml`.
//!
//! MCP: kimi deep-merges the project-level `<ws>/.kimi-code/mcp.json` over the
//! user-level `~/.kimi-code/mcp.json` (verified in the official docs), standard
//! `mcpServers` schema — so writing our entry project-side leaves the user's
//! own servers intact. The entry carries NO per-agent values: kimi spawns
//! stdio MCP children INHERITING its process env (verified live on 0.28), and
//! swarmx puts `SWARMX_AGENT_ID`/`SWARMX_SERVER_URL` in every spawn's env —
//! identity flows through the process tree, the file is identical for every
//! agent, and the shared-workspace last-writer-wins identity bug cannot occur
//! (reasonix/zulu now use the same pattern). Selected by [`super::adapter_for`] for
//! `mcp_format = "kimi-mcp-json"`.
//!
//! Stop hook: kimi has NO project-level hooks — `[[hooks]]` is read only from
//! the user-level `config.toml` (`$KIMI_CODE_HOME`, else `~/.kimi-code`). We
//! patch ONE managed entry there:
//!
//! ```toml
//! [[hooks]]
//! event = "Stop"
//! command = "'/abs/swarmx-mcp' wake-check --server 'http://127.0.0.1:7777' --hook-format kimi"
//! timeout = 10   # kimi's native unit: seconds
//! ```
//!
//! Two deliberate properties:
//!   - It fires for EVERY kimi session on the machine, not just swarmx spawns.
//!     Safe because wake-check resolves `SWARMX_AGENT_ID` (which only swarmx
//!     children carry) and no-ops with exit 0 otherwise — a standalone kimi
//!     session is unaffected.
//!   - kimi's hook protocol is NOT claude's JSON-on-stdout: block = exit code
//!     2 with the continuation message on stderr. Hence the dedicated
//!     `--hook-format kimi` on the command line (see swarmx-mcp wake_check).
//!
//! The patch is string surgery (never a TOML serde round-trip) so the user's
//! comments and hand-arranged sections survive verbatim; it's idempotent and
//! self-heals when the swarmx-mcp binary path drifts (cargo build → packaged
//! .app), replacing the stale `command` line instead of accumulating entries.

use super::shared::{
    home_path, lock_config_patch, render_wake_command, unique_tmp_path, write_json_atomic,
};
use super::{CliAdapter, PreSpawnCtx};
use crate::plugins::CliPlugin;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Zero-sized behavior object for the Kimi Code family.
pub struct KimiAdapter;

impl CliAdapter for KimiAdapter {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn pre_spawn(&self, plugin: &CliPlugin, workspace: &Path, ctx: &PreSpawnCtx) {
        if plugin.auto_inject_mcp {
            if let Err(err) = write_kimi_mcp_json(workspace, &ctx.mcp_bin) {
                tracing::warn!(?err, cli = %plugin.id, "kimi: .kimi-code/mcp.json write failed (agent will lack swarm tools)");
            }
        }
        // Keep our managed .kimi-code/mcp.json out of git's dirty accounting.
        crate::worktree::ignore_managed_artifacts(workspace);
        if plugin.auto_inject_stop_hook {
            if let Err(err) = install_stop_hook(&ctx.mcp_bin, &ctx.server_url, plugin.stop_hook_timeout)
            {
                tracing::warn!(?err, cli = %plugin.id, "kimi stop-hook install failed");
            }
        }
    }

    fn contribute_env(
        &self,
        _plugin: &CliPlugin,
        _agent_id: &str,
        env: &mut HashMap<String, String>,
    ) {
        // The spawn env is rebuilt from an allowlist (spawn.rs: env_clear +
        // FORWARDED_ENV_KEYS) that KIMI_CODE_HOME is NOT on — but the
        // Stop-hook patch above targets kimi_home(), which DOES honor it. A
        // server launched with KIMI_CODE_HOME set would therefore patch
        // <that>/config.toml while the child kimi reads ~/.kimi-code: the
        // wake hook silently never fires. Forward the var verbatim so the
        // child resolves the SAME config root we patched. When unset, insert
        // nothing — the child falls back to ~/.kimi-code via the forwarded
        // HOME, exactly kimi_home()'s own fallback.
        if let Some(dir) = std::env::var("KIMI_CODE_HOME").ok().filter(|v| !v.is_empty()) {
            env.insert("KIMI_CODE_HOME".into(), dir);
        }
    }
}

/// Write `<workspace>/.kimi-code/mcp.json` carrying the swarmx-swarm MCP server
/// in the standard `mcpServers` schema (which kimi reads project-side).
///
/// The entry deliberately carries NO per-agent `args`/`env`: VERIFIED LIVE
/// (kimi 0.28) that kimi spawns stdio MCP children INHERITING its own process
/// environment, and swarmx already puts `SWARMX_AGENT_ID`/`SWARMX_SERVER_URL`
/// in every spawn's env (spawn.rs) — so identity flows per-agent through the
/// process tree, and this file is IDENTICAL for every agent. That's what makes
/// it safe under a shared-workspace layout (same pattern as reasonix/zulu): a
/// sibling's write has the same content, and a respawn reads its OWN env, not a
/// stale file value.
fn write_kimi_mcp_json(workspace: &Path, mcp_bin: &Path) -> Result<()> {
    let dir = workspace.join(".kimi-code");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("mcp.json");
    let body = json!({
        "mcpServers": {
            "swarmx-swarm": {
                "command": mcp_bin.to_string_lossy(),
            }
        }
    });
    write_json_atomic(&path, &body).with_context(|| format!("write {}", path.display()))
}

/// Idempotency key for our managed hook entry: only swarmx renders a hook
/// command carrying this flag, so a `command = "...wake-check --hook-format
/// kimi..."` line is unambiguously ours to rewrite/dedupe.
const HOOK_MARKER: &str = "--hook-format kimi";

/// kimi's data root: `$KIMI_CODE_HOME` when set, else `~/.kimi-code`. Shared
/// by the config.toml patcher and the transcript tailer (sessions live at
/// `<root>/sessions/`). The spawned child sees the SAME root because
/// [`KimiAdapter::contribute_env`] forwards the var — the spawn env is
/// allowlist-rebuilt (spawn.rs), so without that explicit handoff the child
/// would silently fall back to `~/.kimi-code` while we patched the other root.
pub(crate) fn kimi_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KIMI_CODE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home_path().map(|h| h.join(".kimi-code"))
}

/// Where kimi reads `[[hooks]]`: `$KIMI_CODE_HOME/config.toml` when the var is
/// set (forwarded to the child — see kimi_home), else `~/.kimi-code/config.toml`.
fn kimi_config_path() -> Option<PathBuf> {
    kimi_home().map(|h| h.join("config.toml"))
}

fn install_stop_hook(mcp_bin: &Path, server_url: &str, timeout: i64) -> Result<()> {
    let Some(cfg) = kimi_config_path() else {
        return Ok(());
    };
    install_kimi_stop_hook_at(&cfg, mcp_bin, server_url, timeout)
}

fn install_kimi_stop_hook_at(
    cfg: &Path,
    mcp_bin: &Path,
    server_url: &str,
    timeout: i64,
) -> Result<()> {
    let _guard = lock_config_patch();
    // A missing config.toml is fine (kimi creates it on first run; every field
    // is optional) — we create it carrying just our hook so wake works for a
    // first-time kimi user too.
    let existing = match fs::read_to_string(cfg) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", cfg.display())),
    };
    let command = format!(
        "{} {HOOK_MARKER}",
        render_wake_command(mcp_bin, server_url)
    );    let updated = patch_kimi_stop_hook(&existing, &command, timeout);
    if updated == existing {
        return Ok(()); // already current — leave the file (and its mtime) alone
    }
    if let Some(parent) = cfg.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // Atomic write — kimi itself reads this file at session start, so a
    // half-written file would be poison (same hazard class as codex's config).
    let tmp = unique_tmp_path(cfg);
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(updated.as_bytes())?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, cfg).with_context(|| format!("rename to {}", cfg.display()))?;
    Ok(())
}

/// Escape a string for embedding in a TOML basic string (`"..."`). The
/// rendered hook command carries POSIX single quotes (fine verbatim) but a
/// Windows mcp-bin path carries backslashes, which ARE escape introducers in a
/// basic string — they must double. (A TOML literal string can't hold the
/// single quotes, so basic-string + escaping is the only option.)
fn toml_basic_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Splice our managed Stop-hook entry into a kimi `config.toml` body.
/// Pure + string-based so the user's formatting/comments survive and the
/// logic is unit-testable without touching the real config.
///
///  - No prior entry: append a fresh `[[hooks]]` block at EOF (always a valid
///    TOML array-of-tables append, even alongside the user's own `[[hooks]]`
///    entries — kimi runs all matching hooks in parallel).
///  - Prior entry present: rewrite the FIRST managed `command` line in place
///    (heals mcp-bin path drift), DROP any further duplicates. The sibling
///    `event`/`timeout` lines are left untouched.
fn patch_kimi_stop_hook(existing: &str, command: &str, timeout: i64) -> String {
    let command_line = format!("command = \"{}\"", toml_basic_escape(command));
    let mut out = String::with_capacity(existing.len() + command_line.len() + 64);
    let mut written = false;
    for line in existing.split_inclusive('\n') {
        let ours = line.trim_start().starts_with("command") && line.contains(HOOK_MARKER);
        if ours {
            if !written {
                out.push_str(&command_line);
                out.push('\n');
                written = true;
            }
            continue; // drop stale/duplicate copies of our managed line
        }
        out.push_str(line);
    }
    if !written {
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
        }
        out.push_str("[[hooks]]\nevent = \"Stop\"\n");
        out.push_str(&command_line);
        out.push('\n');
        out.push_str(&format!("timeout = {timeout}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn writes_kimi_mcp_json_without_per_agent_values() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let bin = dir.path().join("swarmx-mcp");
        write_kimi_mcp_json(ws, &bin).unwrap();

        let written: Value = serde_json::from_slice(
            &fs::read(ws.join(".kimi-code").join("mcp.json")).unwrap(),
        )
        .unwrap();
        let entry = &written["mcpServers"]["swarmx-swarm"];
        assert_eq!(entry["command"], json!(bin.to_string_lossy().as_ref()));
        // NO args/env: identity flows through the inherited process env
        // (verified live: kimi stdio MCP children inherit SWARMX_*), which is
        // what makes the file safe to share across same-cwd agents.
        assert!(entry.get("args").is_none(), "no per-agent args in the file");
        assert!(entry.get("env").is_none(), "no per-agent env in the file");
    }

    #[test]
    fn hook_appends_fresh_block_to_empty_config() {
        let out = patch_kimi_stop_hook("", "'/a/swarmx-mcp' wake-check --server 'http://x' --hook-format kimi", 10);
        assert!(out.contains("[[hooks]]"));
        assert!(out.contains("event = \"Stop\""));
        assert!(out.contains(HOOK_MARKER));
        assert!(out.contains("timeout = 10"));
        // And the whole body must stay valid TOML with a parseable hooks entry.
        let v: toml::Value = toml::from_str(&out).unwrap();
        let hooks = v["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["event"].as_str().unwrap(), "Stop");
        assert!(hooks[0]["command"].as_str().unwrap().contains(HOOK_MARKER));
        assert_eq!(hooks[0]["timeout"].as_integer().unwrap(), 10);
    }

    #[test]
    fn hook_append_preserves_user_content_verbatim() {
        let existing = "default_model = \"kimi-code/k3\"\n\n# my comment\n[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"lint\"\n";
        let out = patch_kimi_stop_hook(existing, "'/a/swarmx-mcp' wake-check --server 'http://x' --hook-format kimi", 10);
        assert!(out.contains("default_model = \"kimi-code/k3\""));
        assert!(out.contains("# my comment"));
        assert!(out.contains("command = \"lint\""), "user hook preserved");
        let v: toml::Value = toml::from_str(&out).unwrap();
        let hooks = v["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 2, "user entry + ours");
        assert_eq!(hooks[0]["command"].as_str().unwrap(), "lint");
        assert!(hooks[1]["command"].as_str().unwrap().contains(HOOK_MARKER));
    }

    #[test]
    fn hook_patch_is_idempotent() {
        let cmd = "'/a/swarmx-mcp' wake-check --server 'http://x' --hook-format kimi";
        let once = patch_kimi_stop_hook("", cmd, 10);
        let twice = patch_kimi_stop_hook(&once, cmd, 10);
        assert_eq!(once, twice, "second install must be a no-op");
    }

    #[test]
    fn hook_patch_heals_stale_bin_path_without_duplicating() {
        let stale = "[[hooks]]\nevent = \"Stop\"\ncommand = \"'/old/path/swarmx-mcp' wake-check --server 'http://old' --hook-format kimi\"\ntimeout = 10\n";
        let out = patch_kimi_stop_hook(stale, "'/new/swarmx-mcp' wake-check --server 'http://new' --hook-format kimi", 10);
        assert!(!out.contains("/old/path"), "stale path replaced");
        assert!(out.contains("/new/swarmx-mcp"));
        assert_eq!(out.matches(HOOK_MARKER).count(), 1, "exactly one managed entry");
        // No trailing-newline file: still valid.
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["hooks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hook_patch_dedupes_multiple_stale_entries() {
        let stale = "[[hooks]]\nevent = \"Stop\"\ncommand = \"'/old1/swarmx-mcp' wake-check --server 'http://a' --hook-format kimi\"\ntimeout = 5\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"'/old2/swarmx-mcp' wake-check --server 'http://b' --hook-format kimi\"\ntimeout = 5\n";
        let out = patch_kimi_stop_hook(stale, "'/new/swarmx-mcp' wake-check --server 'http://n' --hook-format kimi", 10);
        assert_eq!(out.matches(HOOK_MARKER).count(), 1, "duplicates collapsed");
        assert!(out.contains("/new/swarmx-mcp"));
    }

    #[test]
    fn hook_command_toml_escapes_windows_backslashes() {
        // A Windows mcp-bin path inside the (POSIX-quoted) command must not
        // corrupt the TOML basic string.
        let out = patch_kimi_stop_hook("", "'C:\\tools\\swarmx-mcp' wake-check --server 'http://x' --hook-format kimi", 10);
        let v: toml::Value = toml::from_str(&out).unwrap();
        let cmd = v["hooks"].as_array().unwrap()[0]["command"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(cmd.contains("C:\\tools\\swarmx-mcp"), "backslashes survive: {cmd}");
        assert!(cmd.contains(HOOK_MARKER));
    }

    #[test]
    fn install_creates_missing_config_and_skips_when_current() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let bin = dir.path().join("swarmx-mcp");
        install_kimi_stop_hook_at(&cfg, &bin, "http://127.0.0.1:7777", 10).unwrap();
        let body = fs::read_to_string(&cfg).unwrap();
        assert!(body.contains(HOOK_MARKER));
        let before = fs::read(&cfg).unwrap();
        // Second install with the SAME command: file untouched.
        install_kimi_stop_hook_at(&cfg, &bin, "http://127.0.0.1:7777", 10).unwrap();
        assert_eq!(fs::read(&cfg).unwrap(), before, "no-op when already current");
    }

    #[test]
    fn contribute_env_forwards_kimi_code_home_verbatim() {
        let registry = crate::plugins::PluginRegistry::builtin();
        let plugin = registry.get("kimi").unwrap();
        // Mutates the process-global KIMI_CODE_HOME — kept inside ONE test fn
        // (no sibling test reads or writes the var) so cargo's parallel test
        // runner can't race the mutation.
        std::env::set_var("KIMI_CODE_HOME", "/tmp/swarmx-test-kimi-home");
        let mut env = HashMap::new();
        KimiAdapter.contribute_env(plugin, "kimi-test", &mut env);
        assert_eq!(
            env.get("KIMI_CODE_HOME").map(String::as_str),
            Some("/tmp/swarmx-test-kimi-home"),
            "child env must carry the server's KIMI_CODE_HOME verbatim — else \
             the child reads a different config root than the one we patched"
        );
        // An empty value is kimi_home()'s "unset" — insert nothing.
        std::env::set_var("KIMI_CODE_HOME", "");
        let mut env = HashMap::new();
        KimiAdapter.contribute_env(plugin, "kimi-test", &mut env);
        assert!(!env.contains_key("KIMI_CODE_HOME"), "empty value treated as unset");
        // Unset on the server ⇒ nothing inserted: the child defaults to
        // ~/.kimi-code via the forwarded HOME, kimi_home()'s own fallback.
        std::env::remove_var("KIMI_CODE_HOME");
        let mut env = HashMap::new();
        KimiAdapter.contribute_env(plugin, "kimi-test", &mut env);
        assert!(!env.contains_key("KIMI_CODE_HOME"), "unset ⇒ child falls back via HOME");
    }
}
