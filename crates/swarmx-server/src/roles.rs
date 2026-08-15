//! Role registry — reusable per-agent SOP templates loaded from `roles/`.
//!
//! STATUS: 8 builtin roles ship — `orchestrator` (pulled by the `init`
//! spell) plus 7 worker roles (frontend / backend / reviewer / test-runner /
//! docs-writer / researcher / fixer) that the orchestrator instantiates daily
//! via `swarm_spawn_worker`. The spell-side `role_ref` merge machinery is
//! fully implemented + tested but exercised only by the init spell's
//! orchestrator entry. See `spells.rs` header for the full decision
//! rationale; don't delete it as "dead" nor pad it speculatively.
//!
//! A *role* is the spell-author-facing equivalent of MetaGPT's pinned
//! "PM/Architect/Engineer/QA" team slots: a markdown file under `roles/`
//! captures the default CLI to spawn, a system_prompt template, a handoff
//! signal (which blackboard key the agent writes when done), and a soft
//! artifact_paths convention so the agent knows which part of a shared
//! workspace it owns.
//!
//! Spells reference roles via `role_ref = "frontend"` on a `[[agents]]`
//! entry instead of inlining the prompt verbatim. The runner resolves
//! the ref at spell-launch time, filling in any field the spell didn't
//! override.
//!
//! File shape mirrors `spells/<name>.md`:
//!
//! ```markdown
//! +++
//! id = "frontend"
//! name = "Frontend Engineer"
//! description = "..."
//! default_cli = "claude"
//! artifact_paths = ["apps/frontend/**"]
//! handoff_signal = "frontend.done"
//! # optional W2-1 verify gate; default off — see verify.rs
//! done_checks = ["npm test"]
//! system_prompt_template = """ ... """
//! +++
//!
//! # role docs
//! (free-form body, ignored by parser)
//! ```
//!
//! Bad files are skipped with a `warn!` at load time — never panic.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A typed upstream dependency: "this role consumes the `kind` output of
/// `from_role`". Resolved at spawn time to the producer's *minted* blackboard
/// key (see [`mint_handoff_key`]) so the consumer never hand-types a key.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RoleConsume {
    /// Slug of the upstream role whose output this role waits on.
    pub from_role: String,
    /// Output-kind of that upstream role. Defaults to `"done"`.
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "done".to_string()
}

/// Parsed front-matter for one role file.
#[derive(Debug, Clone, Deserialize)]
pub struct RoleManifest {
    /// Machine-readable identifier. Used as the lookup key when a spell
    /// references this role via `role_ref = "<id>"`.
    pub id: String,
    /// Human-readable display name (for UI / logs).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Which CLI plugin a spell should default to for this role, unless
    /// the spell overrides it explicitly.
    pub default_cli: String,
    /// Soft convention listing the workspace paths this role is allowed
    /// to write. M6a does NOT enforce this — it's repeated inside the
    /// prompt so the LLM honours it. M6b may add a runtime sandbox.
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    /// Blackboard key this role writes when its phase completes.
    /// Recorded here so future tooling (DAG viewer, planner) can
    /// statically reason about role handoffs.
    #[serde(default)]
    pub handoff_signal: String,
    /// Blackboard keys this role is waiting on before it can do real
    /// work. Consumed by the M6b WakeCoordinator: when one of these keys
    /// is written, any agent playing this role gets a mailbox note + a
    /// PTY kick to start a fresh turn. Defaults to empty for roles with
    /// no upstream (frontend, backend).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// W2-1 verify gate (OPT-IN, default empty = off): objective check
    /// commands the server runs in the worker's cwd when it writes its
    /// handoff key, BEFORE the completion is accepted (farewell + auto-kill).
    /// A failed check bounces the delivery back to the worker with the
    /// output tail so it can fix and re-deliver. Every entry is
    /// allowlist-validated at spawn time (`verify::validate_verify_cmd`,
    /// argv exec, never a shell). Empty keeps the legacy behaviour
    /// "handoff write == done" — zero change for existing roles.
    #[serde(default)]
    pub done_checks: Vec<String>,
    /// Template string used as the agent's system_prompt unless the
    /// spell explicitly overrides it. Supports the same `{task}` and
    /// `{<role>_id}` placeholders as a spell's inline system_prompt
    /// (rendered by `spells::render_prompt`).
    pub system_prompt_template: String,

    // ── P0 (F1 角色/任务感知配给) ──────────────────────────────────────
    /// Thin, router-facing descriptor: when SHOULD an orchestrator pick this
    /// role? Kept separate from `system_prompt_template` (worker-facing) so
    /// role selection reads a short hint, not the heavy prompt.
    #[serde(default)]
    pub when_to_use: String,
    /// Default model tier for this role (`opus` | `sonnet` | `haiku`), unless
    /// the spawn overrides it. Placeholder until the P1 capability cards take
    /// over model selection.
    #[serde(default)]
    pub default_model_tier: String,
    /// Typed output-kinds this role produces. The server mints one canonical
    /// blackboard key per kind (see [`mint_handoff_key`]). Empty in the
    /// manifest means "fall back to a single `done` kind at spawn time".
    #[serde(default)]
    pub produces: Vec<String>,
    /// Typed upstream dependencies. Resolved at spawn time against declared /
    /// live producers; the orchestrator usually overrides this per-spawn.
    #[serde(default)]
    pub consumes: Vec<RoleConsume>,

    // ── P1 前向保留(P0 不读,先占位防 schema 漂移) ───────────────────
    /// Tool / MCP allowlist for this role (P1-B hard capability gating).
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    /// Declared modality (`ui` | `backend` | `docs` | `shell` | `research` |
    /// `review` — the enum `validate_manifest` checks against) — a P1-A rule
    /// signal. Declared, never inferred from free text.
    #[serde(default)]
    pub modality: String,
    /// Risk level (`normal` | `high`) — P1-D forces a verifier when high.
    #[serde(default)]
    pub risk: String,
}

#[derive(Debug, Clone)]
pub struct Role {
    pub manifest: RoleManifest,
    #[allow(dead_code)]
    pub source_path: PathBuf,
    #[allow(dead_code)]
    pub markdown_body: String,
}

#[derive(Debug, Clone, Default)]
pub struct RoleRegistry {
    roles: HashMap<String, Role>,
}

impl RoleRegistry {
    /// Walk `dir` for `*.md` files. Each file is parsed independently;
    /// failures log a `warn!` and skip the file without aborting. If
    /// `dir` doesn't exist we return an empty registry — roles are
    /// optional and a fresh checkout shouldn't fail to start the server.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        let mut roles = HashMap::new();
        if !dir.exists() {
            return Ok(Self { roles });
        }
        let read =
            std::fs::read_dir(dir).with_context(|| format!("read_dir({})", dir.display()))?;
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let bytes = match std::fs::read_to_string(&path) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(?err, path = %path.display(), "skip role: read failed");
                    continue;
                }
            };
            let role = match parse_role(&bytes, &path) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(?err, path = %path.display(), "skip role: parse failed");
                    continue;
                }
            };
            if roles.contains_key(&role.manifest.id) {
                tracing::warn!(
                    id = %role.manifest.id,
                    path = %path.display(),
                    "skip role: duplicate id (first one wins)",
                );
                continue;
            }
            roles.insert(role.manifest.id.clone(), role);
        }
        Ok(Self { roles })
    }

    /// Built-in role catalog, compiled into the binary so a deployed server
    /// (whose cwd has no `roles/` dir) still ships a vetted default set. Bad
    /// embeds `warn!` + skip — a parse slip in one role never aborts startup.
    pub fn builtin() -> Self {
        const BUILTIN: &[(&str, &str)] = &[
            (
                "orchestrator.md",
                include_str!("../../../roles/orchestrator.md"),
            ),
            ("frontend.md", include_str!("../../../roles/frontend.md")),
            ("backend.md", include_str!("../../../roles/backend.md")),
            ("reviewer.md", include_str!("../../../roles/reviewer.md")),
            (
                "test-runner.md",
                include_str!("../../../roles/test-runner.md"),
            ),
            (
                "docs-writer.md",
                include_str!("../../../roles/docs-writer.md"),
            ),
            (
                "researcher.md",
                include_str!("../../../roles/researcher.md"),
            ),
            ("fixer.md", include_str!("../../../roles/fixer.md")),
        ];
        let mut roles = HashMap::new();
        for (name, content) in BUILTIN {
            match parse_role(content, Path::new(name)) {
                Ok(role) => {
                    roles.insert(role.manifest.id.clone(), role);
                }
                Err(err) => {
                    tracing::warn!(?err, role = name, "skip builtin role: parse failed");
                }
            }
        }
        Self { roles }
    }

    /// Overlay `other`'s roles onto self, overriding by id (other wins). Used
    /// to layer: built-ins → repo `roles/` dir → project `.swarmx/roles/`.
    pub fn overlay(&mut self, other: RoleRegistry) {
        for (id, role) in other.roles {
            self.roles.insert(id, role);
        }
    }

    pub fn get(&self, id: &str) -> Option<&Role> {
        self.roles.get(id)
    }

    /// All known role ids, sorted — for `unknown role` error messages and the
    /// `swarm_list_roles` tool.
    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<_> = self.roles.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn list(&self) -> Vec<&Role> {
        let mut v: Vec<_> = self.roles.values().collect();
        v.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        v
    }
}

/// Mint the canonical blackboard key a role writes for one output kind. This
/// is the SINGLE source of truth for a handoff key: both the producer's prompt
/// injection and the consumer's resolved `depends_on` derive from it, so the
/// two sides cannot drift (the F3 bug class). Format matches the per-direction
/// blackboard namespace documented in the orchestrator role:
/// `<workspace_id>/<thread_slug>/<role_slug>.<kind>`.
pub fn mint_handoff_key(
    workspace_id: &str,
    thread_slug: &str,
    role_slug: &str,
    kind: &str,
) -> String {
    format!("{workspace_id}/{thread_slug}/{role_slug}.{kind}")
}

/// Locate the `roles/` directory: env override > workspace-relative.
/// Mirrors `spells::default_spells_dir` so roles and spells live side-
/// by-side under the repo root.
pub fn default_roles_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SWARMX_ROLES_DIR") {
        return PathBuf::from(p);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(ws) = manifest.parent().and_then(|p| p.parent()) {
        let candidate = ws.join("roles");
        if candidate.is_dir() {
            return candidate;
        }
    }
    // Last-resort bare-relative fallback. In the packaged app CWD is `/`, so this
    // resolves to `/roles` — which does not exist — and the registry silently
    // overlays NOTHING onto the compiled-in builtins. Tolerable HERE only because
    // the vetted roles are embedded (`RoleRegistry::builtin()`), but a silent miss
    // masks a broken install (env/manifest both failed to resolve). Warn ONCE so
    // release self-checks catch it instead of debugging it live.
    static WARN_ONCE: std::sync::Once = std::sync::Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            "SWARMX_ROLES_DIR unset and CARGO_MANIFEST_DIR-relative `roles/` not found; \
             falling back to CWD-relative `roles` — unreliable under the installed app (CWD=/). \
             Set SWARMX_ROLES_DIR (Tauri sidecar) or rely on the embedded builtin roles."
        );
    });
    PathBuf::from("roles")
}

fn parse_role(content: &str, source_path: &Path) -> Result<Role> {
    // Same `+++` front-matter convention as spells — reuse its parser so the
    // F21 fix (standalone-fence detection; a `+++ b/path` diff line inside a
    // TOML value must not be mistaken for the closing fence) can't diverge.
    let (front_matter, body) = crate::spells::split_front_matter(content)
        .ok_or_else(|| anyhow!("no `+++` front-matter delimiters found"))?;
    let manifest: RoleManifest =
        toml::from_str(front_matter).with_context(|| "parse role front-matter as TOML")?;
    validate_manifest(&manifest)?;
    Ok(Role {
        manifest,
        source_path: source_path.to_path_buf(),
        markdown_body: body.to_string(),
    })
}

fn validate_manifest(m: &RoleManifest) -> Result<()> {
    if m.id.is_empty() {
        return Err(anyhow!("role manifest `id` must be non-empty"));
    }
    if m.default_cli.is_empty() {
        return Err(anyhow!("role manifest `default_cli` must be non-empty"));
    }
    if m.system_prompt_template.is_empty() {
        return Err(anyhow!(
            "role manifest `system_prompt_template` must be non-empty"
        ));
    }
    validate_optional_enum(
        "default_model_tier",
        &m.default_model_tier,
        &["opus", "sonnet", "haiku"],
    )?;
    validate_optional_enum(
        "modality",
        &m.modality,
        &["ui", "backend", "docs", "shell", "research", "review"],
    )?;
    validate_optional_enum("risk", &m.risk, &["normal", "high"])?;
    Ok(())
}

fn validate_optional_enum(field: &str, value: &str, allowed: &[&str]) -> Result<()> {
    let v = value.trim();
    if v.is_empty() || allowed.contains(&v) {
        return Ok(());
    }
    Err(anyhow!(
        "role manifest `{field}` must be one of {allowed:?}; got `{v}`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn parse_role_minimal() {
        let src = r#"+++
id = "frontend"
name = "Frontend Engineer"
default_cli = "claude"
system_prompt_template = "You are FE. Task: {task}."
+++

# docs body
"#;
        let r = parse_role(src, Path::new("/tmp/frontend.md")).unwrap();
        assert_eq!(r.manifest.id, "frontend");
        assert_eq!(r.manifest.default_cli, "claude");
        assert!(r.manifest.system_prompt_template.contains("{task}"));
        assert!(r.markdown_body.contains("# docs body"));
        assert!(r.manifest.depends_on.is_empty(), "default empty");
    }

    #[test]
    fn parse_role_with_depends_on() {
        let src = r#"+++
id = "test"
default_cli = "claude"
depends_on = ["frontend.done", "backend.done"]
system_prompt_template = "you are test"
+++"#;
        let r = parse_role(src, Path::new("/tmp/test.md")).unwrap();
        assert_eq!(
            r.manifest.depends_on,
            vec!["frontend.done".to_string(), "backend.done".to_string()]
        );
    }

    #[test]
    fn parse_role_rejects_missing_id() {
        let src = r#"+++
default_cli = "claude"
system_prompt_template = "x"
+++"#;
        let err = parse_role(src, Path::new("/tmp/x.md")).unwrap_err();
        // toml deserialize fails because `id` is required (no #[serde(default)])
        assert!(format!("{err:#}").to_lowercase().contains("id"));
    }

    #[test]
    fn parse_role_rejects_missing_default_cli() {
        let src = r#"+++
id = "x"
system_prompt_template = "x"
+++"#;
        let err = parse_role(src, Path::new("/tmp/x.md")).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("default_cli"));
    }

    #[test]
    fn parse_role_rejects_empty_template() {
        let src = r#"+++
id = "x"
default_cli = "claude"
system_prompt_template = ""
+++"#;
        let err = parse_role(src, Path::new("/tmp/x.md")).unwrap_err();
        assert!(
            format!("{err:#}").contains("system_prompt_template"),
            "got: {err:#}"
        );
    }

    #[test]
    fn registry_loads_only_md_files_and_skips_bad_ones() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "good.md",
            r#"+++
id = "good"
default_cli = "claude"
system_prompt_template = "ok"
+++
"#,
        );
        write(dir.path(), "bad.md", "no front matter at all");
        write(
            dir.path(),
            "ignored.txt",
            "+++\nid=\"x\"\ndefault_cli=\"y\"\nsystem_prompt_template=\"z\"\n+++",
        );

        let reg = RoleRegistry::load_dir(dir.path()).unwrap();
        let ids: Vec<_> = reg.list().iter().map(|r| r.manifest.id.clone()).collect();
        assert_eq!(ids, vec!["good".to_string()]);
    }

    #[test]
    fn registry_load_dir_missing_returns_empty() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let reg = RoleRegistry::load_dir(&nonexistent).unwrap();
        assert_eq!(reg.list().len(), 0);
    }

    #[test]
    fn registry_deduplicates_by_id() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "a.md",
            "+++\nid=\"same\"\ndefault_cli=\"claude\"\nsystem_prompt_template=\"a\"\n+++",
        );
        write(
            dir.path(),
            "b.md",
            "+++\nid=\"same\"\ndefault_cli=\"codex\"\nsystem_prompt_template=\"b\"\n+++",
        );
        let reg = RoleRegistry::load_dir(dir.path()).unwrap();
        assert_eq!(reg.list().len(), 1, "duplicate id should be deduped");
    }

    #[test]
    fn registry_get_by_id() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "frontend.md",
            "+++\nid=\"frontend\"\ndefault_cli=\"claude\"\nsystem_prompt_template=\"x\"\n+++",
        );
        let reg = RoleRegistry::load_dir(dir.path()).unwrap();
        assert!(reg.get("frontend").is_some());
        assert!(reg.get("backend").is_none());
    }

    // ── P0: typed handoff key + extended schema ──────────────────────────

    #[test]
    fn mint_handoff_key_format() {
        assert_eq!(
            mint_handoff_key("ws_ab12", "dark-mode", "frontend", "done"),
            "ws_ab12/dark-mode/frontend.done"
        );
        // Deterministic / idempotent: same inputs → same key (no Date/rand).
        assert_eq!(
            mint_handoff_key("w", "main", "backend", "spec"),
            mint_handoff_key("w", "main", "backend", "spec")
        );
    }

    #[test]
    fn manifest_new_fields_default_when_absent() {
        // An old-style minimal role (no P0 fields) must still parse, with the
        // new fields defaulting empty — backward compatibility.
        let src = r#"+++
id = "legacy"
default_cli = "claude"
system_prompt_template = "x"
+++"#;
        let r = parse_role(src, Path::new("/tmp/legacy.md")).unwrap();
        assert!(r.manifest.when_to_use.is_empty());
        assert!(r.manifest.default_model_tier.is_empty());
        assert!(r.manifest.produces.is_empty());
        assert!(r.manifest.consumes.is_empty());
        assert!(r.manifest.tool_allowlist.is_empty());
        assert!(r.manifest.modality.is_empty());
        // W2-1: the verify gate defaults OFF.
        assert!(r.manifest.done_checks.is_empty());
    }

    #[test]
    fn manifest_parses_done_checks() {
        let src = r#"+++
id = "backend"
default_cli = "claude"
done_checks = ["cargo test", "cargo clippy"]
system_prompt_template = "x"
+++"#;
        let r = parse_role(src, Path::new("/tmp/b.md")).unwrap();
        assert_eq!(
            r.manifest.done_checks,
            vec!["cargo test".to_string(), "cargo clippy".to_string()]
        );
    }

    #[test]
    fn manifest_parses_produces_and_consumes() {
        let src = r#"+++
id = "frontend"
default_cli = "claude"
default_model_tier = "sonnet"
when_to_use = "ui work"
produces = ["done", "spec"]
consumes = [{ from_role = "designer", kind = "spec" }, { from_role = "planner" }]
system_prompt_template = "x"
+++"#;
        let r = parse_role(src, Path::new("/tmp/f.md")).unwrap();
        assert_eq!(r.manifest.produces, vec!["done", "spec"]);
        assert_eq!(r.manifest.default_model_tier, "sonnet");
        assert_eq!(r.manifest.consumes.len(), 2);
        assert_eq!(r.manifest.consumes[0].from_role, "designer");
        assert_eq!(r.manifest.consumes[0].kind, "spec");
        // kind omitted → defaults to "done"
        assert_eq!(r.manifest.consumes[1].from_role, "planner");
        assert_eq!(r.manifest.consumes[1].kind, "done");
    }

    #[test]
    fn manifest_rejects_unknown_capability_metadata() {
        let src = concat!(
            "+++\n",
            "id = \"frontend\"\n",
            "default_cli = \"claude\"\n",
            "modality = \"whatever\"\n",
            "system_prompt_template = \"x\"\n",
            "+++\n",
        );
        let err = parse_role(src, Path::new("/tmp/f.md")).unwrap_err();
        assert!(format!("{err:#}").contains("modality"), "got: {err:#}");
    }

    #[test]
    fn overlay_overrides_by_id() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "frontend.md",
            "+++\nid=\"frontend\"\ndefault_cli=\"codex\"\nsystem_prompt_template=\"override\"\n+++",
        );
        let mut base = RoleRegistry::builtin();
        let before = base.get("frontend").unwrap().manifest.default_cli.clone();
        assert_eq!(before, "claude", "builtin frontend defaults to claude");
        base.overlay(RoleRegistry::load_dir(dir.path()).unwrap());
        assert_eq!(
            base.get("frontend").unwrap().manifest.default_cli,
            "codex",
            "dir overlay wins by id"
        );
    }

    #[test]
    fn builtin_ships_the_vetted_set() {
        let reg = RoleRegistry::builtin();
        for id in [
            "orchestrator",
            "frontend",
            "backend",
            "reviewer",
            "test-runner",
            "docs-writer",
            "researcher",
            "fixer",
        ] {
            assert!(reg.get(id).is_some(), "builtin role `{id}` should load");
        }
        // Built-in worker roles declare a typed `produces`.
        assert_eq!(reg.get("frontend").unwrap().manifest.produces, vec!["done"]);
        assert_eq!(reg.get("backend").unwrap().manifest.default_cli, "codex");
        // W2-1: every builtin ships with the verify gate OFF — opting in is a
        // per-project decision (`.swarmx/roles/` overlay), so the default
        // "handoff write == done" behaviour never changes for existing users.
        for r in reg.list() {
            assert!(
                r.manifest.done_checks.is_empty(),
                "builtin role `{}` must not enable done_checks by default",
                r.manifest.id
            );
        }
    }
}
