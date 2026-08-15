//! Reasonix adapter — DeepSeek-native coding agent driven over `reasonix serve`.
//!
//! reasonix is the odd one out: it is NOT typed into a TUI (claude/codex) nor
//! driven over a TUI's side-channel (opencode). It runs as a headless HTTP+SSE
//! session server (`reasonix serve`), and swarmx drives it entirely over that
//! API — POST `/submit` for bootstrap/wakes, the `/events` SSE stream for
//! turn-end/activity/usage (see `crate::reasonix_serve`). The PTY this adapter's
//! agent still gets only carries `serve`'s one-line startup banner; lifecycle /
//! kill / is_alive reuse the existing PTY machinery unchanged.
//!
//! Pre-spawn this adapter does exactly two things:
//!   1. Drop a project-root `<ws>/.mcp.json` (Claude Code `mcpServers` schema —
//!      reasonix reads it as-is; verified live that it exposes
//!      `mcp__swarmx-swarm__swarm_*` to the model). The entry carries NO
//!      per-agent identity — same kimi pattern: stdio MCP children inherit
//!      `SWARMX_AGENT_ID` / `SWARMX_SERVER_URL` from the serve process env
//!      (set by spawn.rs), so Shared-layout last-writer-wins cannot scramble
//!      identity.
//!   2. Point reasonix at a per-agent `REASONIX_HOME` (env) so its sessions /
//!      config don't collide across agents. No config.toml is written: reasonix
//!      ships built-in `deepseek-flash`/`deepseek-pro` providers and reads the
//!      key from `DEEPSEEK_API_KEY` (forwarded by spawn.rs); tool auto-approval
//!      is set at runtime via `/tool-approval-mode {yolo}` in `reasonix_serve`.
//!
//! No trust patch (reasonix has no folder-trust gate) and no Stop hook (its Stop
//! event is observe-only — the SSE `turn_done` is the turn-end signal instead).
//!
//! Selected by [`super::adapter_for`] for `mcp_format = "reasonix-mcp-json"`.

use super::shared::{home_path, write_json_atomic};
use super::{CliAdapter, PreSpawnCtx};
use crate::plugins::CliPlugin;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Zero-sized behavior object for the Reasonix family.
pub struct ReasonixAdapter;

impl CliAdapter for ReasonixAdapter {
    fn name(&self) -> &'static str {
        "reasonix"
    }

    fn pre_spawn(&self, plugin: &CliPlugin, workspace: &Path, ctx: &PreSpawnCtx) {
        if plugin.auto_inject_mcp {
            if let Err(err) = write_workspace_mcp_json(workspace, &ctx.mcp_bin) {
                tracing::warn!(?err, cli = %plugin.id, "reasonix: .mcp.json write failed (agent will lack swarm tools)");
            }
        }
        // Keep our managed .mcp.json out of git's dirty accounting (same as the
        // claude/codex managed-artifact handling).
        crate::worktree::ignore_managed_artifacts(workspace);
    }

    fn contribute_env(
        &self,
        _plugin: &CliPlugin,
        agent_id: &str,
        env: &mut HashMap<String, String>,
    ) {
        // Per-agent REASONIX_HOME so sessions/config stay isolated across agents.
        // reasonix falls back to its compiled-in providers when this dir has no
        // config.toml, so an empty home is fine (key comes from DEEPSEEK_API_KEY).
        match reasonix_home_path(agent_id) {
            Some(home) => {
                if let Err(err) = fs::create_dir_all(&home) {
                    tracing::warn!(?err, agent = %agent_id, "reasonix: could not create REASONIX_HOME; falling back to ~/.reasonix");
                    return;
                }
                env.insert(
                    "REASONIX_HOME".into(),
                    home.to_string_lossy().into_owned(),
                );
            }
            None => tracing::warn!(agent = %agent_id, "reasonix: no $HOME; REASONIX_HOME not isolated"),
        }
    }
}

/// Per-agent `REASONIX_HOME` under `~/.swarmx/reasonix-home/<agent_id>`.
/// `None` if `$HOME` is unset.
fn reasonix_home_path(agent_id: &str) -> Option<PathBuf> {
    home_path().map(|h| h.join(".swarmx").join("reasonix-home").join(agent_id))
}

/// Write `<workspace>/.mcp.json` carrying the swarmx-swarm MCP server in the
/// Claude Code `mcpServers` schema (which reasonix reads verbatim).
///
/// Deliberately carries NO `--agent-id` / `env` block: swarmx-mcp accepts
/// `SWARMX_AGENT_ID` / `SWARMX_SERVER_URL` from the process environment
/// (clap `env = ...`), and spawn.rs injects both into every agent child.
/// Matching kimi's shared-file pattern means Shared-layout peers can overwrite
/// the same path without scrambling each other's identity.
fn write_workspace_mcp_json(workspace: &Path, mcp_bin: &Path) -> Result<()> {
    let path = workspace.join(".mcp.json");
    let body = json!({
        "mcpServers": {
            "swarmx-swarm": {
                "type": "stdio",
                "command": mcp_bin.to_string_lossy(),
            }
        }
    });
    write_json_atomic(&path, &body).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::tempdir;

    #[test]
    fn writes_mcp_json_without_embedded_agent_identity() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let bin = dir.path().join("swarmx-mcp");
        write_workspace_mcp_json(ws, &bin).unwrap();

        let written: Value =
            serde_json::from_slice(&fs::read(ws.join(".mcp.json")).unwrap()).unwrap();
        let entry = &written["mcpServers"]["swarmx-swarm"];
        assert_eq!(entry["type"], json!("stdio"));
        assert_eq!(entry["command"], json!(bin.to_string_lossy().as_ref()));
        assert!(entry.get("args").is_none());
        assert!(entry.get("env").is_none());
    }

    #[test]
    fn shared_layout_overwrite_keeps_identical_content() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let bin = dir.path().join("swarmx-mcp");
        write_workspace_mcp_json(ws, &bin).unwrap();
        let first = fs::read(ws.join(".mcp.json")).unwrap();
        write_workspace_mcp_json(ws, &bin).unwrap();
        let second = fs::read(ws.join(".mcp.json")).unwrap();
        assert_eq!(first, second);
    }
}
