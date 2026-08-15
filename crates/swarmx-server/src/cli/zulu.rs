//! zulu (Comate) CLI adapter — writes the swarm MCP config where zulu reads it,
//! and isolates each agent's `zulu serve` into its own `HOME`.
//!
//! Unlike reasonix (root `<ws>/.mcp.json`), zulu reads its MCP servers from
//! `<ws>/.comate/mcp.json` (verified: the Comate kernel's `MCP_SETTINGS_DIR=
//! ".comate" / mcp.json` — `zulu inspect` picks up `swarmx-swarm` from there).
//! Same standard `mcpServers` schema (`{"mcpServers":{...}}`). The entry
//! carries NO per-agent identity (kimi pattern): stdio MCP children inherit
//! `SWARMX_AGENT_ID` / `SWARMX_SERVER_URL` from the serve process env.
//! Selected by [`super::adapter_for`] for `mcp_format = "zulu-mcp-json"`.
//!
//! ## Why each zulu agent needs its OWN `HOME` (the serve-singleton fix)
//!
//! `zulu serve` is a machine-wide SINGLETON: on launch it reads
//! `$HOME/.comate/zulu-serve.pid` (`findExistingServe` in the v1.6.1 bundle) and,
//! if that pid is alive + `/health` answers, prints `{...,port:<EXISTING>,
//! reused:true}` and EXITS 0 instead of binding the port swarmx asked for. swarmx
//! allocates a fresh port + launches a serve PER AGENT, so the 2nd+ concurrent
//! zulu agent coalesces onto the 1st's serve: its shim exits (looks like the turn
//! ended) and its [`crate::zulu_serve`] driver polls a DEAD allocated port →
//! "serve never came up" → the agent is wedged forever. That silently breaks
//! every multi-zulu scenario (a fusion panel's 2nd+ contestant, the judge after
//! zulu contestants, any two zulu workers).
//!
//! Fix: point each agent's `HOME` at a per-agent dir so its serve reads a private
//! (empty) `zulu-serve.pid` and binds ITS allocated port — no coalescing. Auth is
//! the explicit `-l <license>` (also in every POST body), which is
//! HOME-independent; and we best-effort symlink the real `~/.comate` credentials
//! (login-user / cli config) into the isolated HOME so an ambient `zulu login`
//! keeps working too — everything EXCEPT the serve-state files (`zulu-serve.pid`,
//! `ctrlserver`), which MUST stay per-agent or the singleton reappears. Mirrors
//! codex's per-agent `~/.swarmx/codex-home/<id>` convention (no cleanup — the
//! dir is tiny and shares that family's accretion tradeoff).

use super::shared::{home_path, write_json_atomic};
use super::{CliAdapter, PreSpawnCtx};
use crate::plugins::CliPlugin;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Zero-sized behavior object for the zulu (Comate) family.
pub struct ZuluAdapter;

impl CliAdapter for ZuluAdapter {
    fn name(&self) -> &'static str {
        "zulu"
    }

    fn pre_spawn(&self, plugin: &CliPlugin, workspace: &Path, ctx: &PreSpawnCtx) {
        if plugin.auto_inject_mcp {
            if let Err(err) = write_comate_mcp_json(workspace, &ctx.mcp_bin) {
                tracing::warn!(?err, cli = %plugin.id, "zulu: .comate/mcp.json write failed (agent will lack swarm tools)");
            }
        }
        // Keep our managed .comate/mcp.json out of git's dirty accounting.
        crate::worktree::ignore_managed_artifacts(workspace);
    }

    fn contribute_env(
        &self,
        _plugin: &CliPlugin,
        agent_id: &str,
        env: &mut HashMap<String, String>,
    ) {
        // Isolate this agent's HOME so its `zulu serve` gets a private
        // `zulu-serve.pid` and binds its OWN allocated port (see module docs).
        let Some(iso) = isolated_home(agent_id) else {
            return;
        };
        if let Err(err) = seed_isolated_home(env.get("HOME").map(Path::new), &iso) {
            // Best-effort: even a bare isolated HOME works via the `-l` license.
            // Never fail the spawn — at worst a `zulu login`-only user loses cred
            // reuse (license path is unaffected).
            tracing::warn!(?err, agent = %agent_id, "zulu: isolated HOME seed failed; falling back to bare isolated HOME (license auth still works)");
        }
        let iso = iso.to_string_lossy().into_owned();
        // os.homedir() reads HOME on unix, USERPROFILE on Windows — set both.
        env.insert("USERPROFILE".into(), iso.clone());
        env.insert("HOME".into(), iso.clone());
        tracing::info!(agent = %agent_id, home = %iso, "zulu per-agent HOME injected (isolates zulu serve singleton so each agent binds its own port)");
    }
}

/// Per-agent isolated HOME: `~/.swarmx/zulu-home/<agent_id>`. `None` if the
/// server has no HOME to anchor it (then the real HOME is left untouched — a
/// single zulu agent still works; only multi-zulu needs the isolation).
fn isolated_home(agent_id: &str) -> Option<PathBuf> {
    home_path().map(|h| h.join(".swarmx").join("zulu-home").join(agent_id))
}

/// Create the isolated HOME and best-effort seed its `.comate` with the real
/// user's Comate credentials — symlinking EVERY real `~/.comate` entry EXCEPT
/// the serve-state files that must stay per-agent (`zulu-serve.pid`,
/// `ctrlserver`). `real_home` is where the user's real `~/.comate` lives (the
/// HOME the generic env set before this runs); `None`/missing → nothing to seed
/// (bare isolated HOME, license auth).
fn seed_isolated_home(real_home: Option<&Path>, iso: &Path) -> Result<()> {
    let comate = iso.join(".comate");
    std::fs::create_dir_all(&comate)
        .with_context(|| format!("create {}", comate.display()))?;

    let Some(real_comate) = real_home.map(|h| h.join(".comate")) else {
        return Ok(());
    };
    if !real_comate.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&real_comate)
        .with_context(|| format!("read {}", real_comate.display()))?
    {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        // Skip the singleton's serve-state — linking these would re-share the
        // pidfile/ctrl socket and collapse every agent back onto one serve.
        if name == *"zulu-serve.pid" || name == *"ctrlserver" {
            continue;
        }
        let link = comate.join(&name);
        if link.exists() {
            continue;
        }
        // Per-entry best-effort: a failed symlink just drops that one cred.
        if let Err(err) = symlink_path(&entry.path(), &link) {
            tracing::debug!(?err, link = %link.display(), "zulu: cred symlink skipped");
        }
    }
    Ok(())
}

/// Symlink `src` → `dst`. Unix only; on other platforms the isolated HOME stays
/// bare (license auth covers it — see module docs).
#[cfg(unix)]
fn symlink_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(not(unix))]
fn symlink_path(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Write `<workspace>/.comate/mcp.json` carrying the swarmx-swarm MCP server in
/// the standard `mcpServers` schema (which the Comate kernel reads).
///
/// No embedded `--agent-id` / `env`: identity flows from the serve child's
/// process environment (`SWARMX_*` set by spawn.rs), matching kimi/reasonix
/// so Shared-layout overwrites cannot scramble peer identity.
fn write_comate_mcp_json(workspace: &Path, mcp_bin: &Path) -> Result<()> {
    let dir = workspace.join(".comate");
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn writes_comate_mcp_json_without_embedded_agent_identity() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let bin = dir.path().join("swarmx-mcp");
        write_comate_mcp_json(ws, &bin).unwrap();

        // Written under .comate/, NOT the root .mcp.json.
        assert!(!ws.join(".mcp.json").exists());
        let written: Value =
            serde_json::from_slice(&fs::read(ws.join(".comate").join("mcp.json")).unwrap()).unwrap();
        let entry = &written["mcpServers"]["swarmx-swarm"];
        assert_eq!(entry["command"], json!(bin.to_string_lossy().as_ref()));
        assert!(entry.get("args").is_none());
        assert!(entry.get("env").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn seed_isolated_home_links_creds_but_never_serve_state() {
        let root = tempdir().unwrap();
        // A realistic real ~/.comate: creds + config to reuse, serve-state to skip.
        let real_home = root.path().join("real");
        let real_comate = real_home.join(".comate");
        fs::create_dir_all(real_comate.join("cli")).unwrap();
        fs::write(real_comate.join("login-user"), b"tok").unwrap();
        fs::write(real_comate.join("cli").join("config.json"), b"{}").unwrap();
        fs::write(real_comate.join("zulu-serve.pid"), b"{\"pid\":1}").unwrap();
        fs::create_dir_all(real_comate.join("ctrlserver")).unwrap();

        let iso = root.path().join("iso-home");
        seed_isolated_home(Some(&real_home), &iso).unwrap();

        let iso_comate = iso.join(".comate");
        // Creds/config are reachable through the isolated HOME…
        assert_eq!(fs::read(iso_comate.join("login-user")).unwrap(), b"tok");
        assert!(iso_comate.join("cli").join("config.json").is_file());
        // …but the serve-state is NOT linked — that's what keeps each serve on
        // its own port (the whole point of the isolation).
        assert!(!iso_comate.join("zulu-serve.pid").exists());
        assert!(!iso_comate.join("ctrlserver").exists());
    }

    #[test]
    fn seed_isolated_home_tolerates_missing_real_comate() {
        // Brand-new / license-only user: no ~/.comate to seed from. Must still
        // create the isolated .comate (bare) without erroring.
        let root = tempdir().unwrap();
        let iso = root.path().join("iso-home");
        seed_isolated_home(Some(&root.path().join("no-such-home")), &iso).unwrap();
        assert!(iso.join(".comate").is_dir());
    }

    #[test]
    fn isolated_home_is_per_agent_under_swarmx() {
        // Tolerate a HOME-less CI env: the composition only matters when anchored.
        let (Some(a), Some(b)) = (isolated_home("zulu-aaa"), isolated_home("zulu-bbb")) else {
            return;
        };
        assert_ne!(a, b);
        assert!(a.ends_with(".swarmx/zulu-home/zulu-aaa"));
    }
}
