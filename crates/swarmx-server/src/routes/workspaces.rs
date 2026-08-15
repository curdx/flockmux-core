//! Workspace, root, and direction REST endpoints.
//!
//! This module owns the workspace-as-first-class surface and the git-worktree
//! direction lifecycle. Agent spawning and spell execution stay in `rest`; this
//! module calls those explicit seams when a workspace action needs them.

use super::rest::{
    run_spell, spawn_bootstrap_inject, spawn_with_bookkeeping, teardown_agent, BootstrapCtx,
};
use crate::spawn::WorkspaceLayout;
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use swarmx_protocol::rest::{
    BranchInfo, CreateThreadRequest, CreateWorkspaceRequest, RunSpellRequest, ThreadInfo,
    UpdateThreadRequest, Workspace, WorkspaceRoot,
};
use swarmx_protocol::ws_swarm::SwarmEvent;
use swarmx_storage::{NewThread, NewWorkspace, NewWorkspaceRoot, ThreadRecord};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Map a storage `ThreadRecord` onto the wire `ThreadInfo` (drops the internal
/// `deleted_at`; the API only ever surfaces alive threads).
fn thread_record_to_info(t: ThreadRecord) -> ThreadInfo {
    ThreadInfo {
        id: t.id,
        workspace_id: t.workspace_id,
        slug: t.slug,
        name: t.name,
        isolation: t.isolation,
        branch: t.branch,
        cwd: t.cwd,
        state: t.state,
        model_tier: t.model_tier,
        reasoning_effort: t.reasoning_effort,
        dirty: false, // live value folded in by list_workspaces_handler
        ahead: None,  // live values folded in by list_workspaces_handler
        behind: None,
        created_at: t.created_at,
    }
}

// ────────────────────────────────────────────────────────────────────────
// Workspace endpoints (Step 2 of workspace-as-first-class rollout)
// ────────────────────────────────────────────────────────────────────────

/// `POST /api/workspaces` — create a new workspace and return the
/// persisted row. CreateWizard calls this *before* launching the `init`
/// spell so the spell's spawned scout already carries `workspace_id`.
pub async fn create_workspace_handler(
    State(state): State<AppState>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<Json<Workspace>, (StatusCode, Json<serde_json::Value>)> {
    if req.name.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name must be non-empty"})),
        ));
    }
    if req.cwd.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "cwd must be non-empty"})),
        ));
    }
    // Validate the cwd BEFORE persisting the row. Otherwise we'd create the
    // workspace, then the `init` spell's "create shared workspace" step fails
    // because the directory can't be entered — leaving the user with a dead,
    // 0-member ghost workspace pointing at a bad path. A 4xx here keeps the DB
    // clean and surfaces a plain "doesn't exist" message instead of a 500.
    {
        let path = std::path::Path::new(req.cwd.trim());
        if !path.exists() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("directory does not exist: {}", req.cwd.trim())})),
            ));
        }
        if !path.is_dir() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("not a directory: {}", req.cwd.trim())})),
            ));
        }
    }
    let rec = state
        .store
        .create_workspace(
            NewWorkspace {
                name: req.name,
                cwd: req.cwd,
                accent: req.accent,
            },
            now_ms(),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;

    // Auto-create the workspace's `main` direction so every workspace owns at
    // least one thread from birth. Zero-friction: shared isolation, cwd = the
    // workspace cwd, ready immediately. A store failure here is non-fatal —
    // agents simply fall back to the legacy `thread_id = None` (= main) path —
    // so we log and return an empty thread list rather than 500 the creation.
    let threads: Vec<ThreadInfo> = match state
        .store
        .create_thread(
            NewThread {
                workspace_id: rec.id.clone(),
                slug: "main".to_string(),
                name: Some("main".to_string()),
                isolation: "shared".to_string(),
                branch: None,
                cwd: rec.cwd.clone(),
                state: "ready".to_string(),
            },
            now_ms(),
        )
        .await
    {
        Ok(t) => vec![thread_record_to_info(t)],
        Err(e) => {
            tracing::warn!(?e, workspace = %rec.id, "create main thread failed; agents fall back to main = None");
            Vec::new()
        }
    };

    // Attach any dependency-source roots the wizard sent. Each is validated
    // the same way as the primary cwd above (exists + is a dir → 4xx), then
    // persisted. The workspace row already exists at this point; a failed root
    // insert returns 500 without rolling back the workspace (acceptable — the
    // user can re-attach the root). Empty/whitespace paths are skipped.
    let mut roots: Vec<WorkspaceRoot> = Vec::new();
    for root in req.roots {
        let p = root.path.trim();
        if p.is_empty() {
            continue;
        }
        let path = std::path::Path::new(p);
        if !path.exists() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("dependency directory does not exist: {}", p)})),
            ));
        }
        if !path.is_dir() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("dependency path is not a directory: {}", p)})),
            ));
        }
        // The wizard only ever creates the primary + peers + under-primary
        // deps, so every root it sends is a top-level node (parent_id=None).
        // Any client-supplied id is ignored — the server mints it.
        let saved = state
            .store
            .add_workspace_root(
                NewWorkspaceRoot {
                    workspace_id: rec.id.clone(),
                    path: p.to_string(),
                    role: root.role,
                    label: root.label,
                    parent_id: None,
                },
                now_ms(),
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            })?;
        roots.push(WorkspaceRoot {
            id: saved.id,
            path: saved.path,
            role: saved.role,
            label: saved.label,
            parent_id: saved.parent_id,
            branch: None, // filled at list time
        });
    }

    // If any tree nodes were attached, write a swarmx-managed context block
    // into the primary project dir so the spawned orchestrator (claude →
    // CLAUDE.md, codex → AGENTS.md) reads the attached source directly instead
    // of decompiling/guessing. Best-effort; never fatal.
    if !roots.is_empty() {
        write_workspace_deps_context(rec.cwd.trim(), &rec.name, &roots);
    }

    Ok(Json(Workspace {
        id: rec.id,
        slug: rec.slug,
        name: rec.name,
        cwd: rec.cwd,
        cwd_branch: None, // filled at list time
        accent: rec.accent,
        created_at: rec.created_at,
        member_count: 0,
        roots,
        threads,
    }))
}

/// Write/refresh a swarmx-managed "workspace structure" block into the
/// workspace's CLAUDE.md and AGENTS.md so the orchestrator reads the attached
/// source directly (best practice: a per-project context file) instead of
/// decompiling/guessing. The block renders the workspace's user-defined
/// LOGICAL tree: the primary project (`cwd` + `name`) plus every attached
/// node, nested by `parent_id`. Idempotent: the block is delimited by
/// HTML-comment markers and replaced in place on re-write; any user content
/// outside the markers is preserved. When `roots` is empty the managed block
/// is STRIPPED instead of written (the inverse path — used when the last
/// attached node is removed), leaving any surrounding user content intact.
/// Best-effort — failures are logged, never fatal.
fn write_workspace_deps_context(cwd: &str, name: &str, roots: &[WorkspaceRoot]) {
    use std::fmt::Write as _;
    const START: &str = "<!-- swarmx:deps:start -->";
    const END: &str = "<!-- swarmx:deps:end -->";

    // No roots left → strip the managed block (and trailing blank lines)
    // from each context file if present. We never create a file here; only
    // existing files with a managed block are rewritten.
    if roots.is_empty() {
        for fname in ["CLAUDE.md", "AGENTS.md"] {
            let path = std::path::Path::new(cwd).join(fname);
            let existing = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue, // file doesn't exist / unreadable — nothing to strip
            };
            if let (Some(s), Some(e)) = (existing.find(START), existing.find(END)) {
                let end_full = e + END.len();
                // Drop the block plus any trailing newlines that followed it
                // so we don't leave a dangling blank gap behind.
                let after = existing[end_full..].trim_start_matches(['\n', '\r']);
                let before = &existing[..s];
                // If the block was the only content, `before` is empty / blank
                // and the file becomes empty — that's fine per spec.
                let stripped = if after.is_empty() {
                    before.trim_end().to_string()
                } else {
                    format!("{}{}", before, after)
                };
                if stripped.trim().is_empty() {
                    // The managed block was the file's only content — delete the
                    // now-empty file instead of leaving a dangling 0-byte
                    // CLAUDE.md/AGENTS.md behind (swarmx created it, swarmx
                    // cleans it up). If the user had their own content around the
                    // block, `stripped` is non-empty and we keep+rewrite it.
                    match std::fs::remove_file(&path) {
                        Ok(()) => {
                            tracing::info!(file = %path.display(), "removed empty workspace deps context file (no roots left)")
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            tracing::warn!(?e, file = %path.display(), "failed removing empty workspace deps context file")
                        }
                    }
                } else if let Err(e) = std::fs::write(&path, stripped) {
                    tracing::warn!(?e, file = %path.display(), "failed stripping workspace deps context");
                } else {
                    tracing::info!(file = %path.display(), "stripped workspace deps context (no roots left)");
                }
            }
        }
        return;
    }

    // Render the prefix label for one tree node by role.
    fn node_label(role: &str) -> &'static str {
        match role {
            "project" => "项目",
            "tool" => "[工具]",
            _ => "[依赖]",
        }
    }
    // Emit `node` (and recurse into its children) at the given depth. Children
    // are the roots whose parent_id == node.id, in slice order (already sorted
    // by created_at by the caller). `depth` controls the 2-space indent.
    fn emit_node(block: &mut String, node: &WorkspaceRoot, roots: &[WorkspaceRoot], depth: usize) {
        let indent = "  ".repeat(depth);
        let label = node_label(&node.role);
        let name = node.label.as_deref().unwrap_or("");
        let _ = if name.is_empty() {
            writeln!(block, "{indent}- {label} `{}`", node.path)
        } else {
            writeln!(block, "{indent}- {label} {name} `{}`", node.path)
        };
        for child in roots
            .iter()
            .filter(|r| r.parent_id.as_deref() == Some(node.id.as_str()))
        {
            emit_node(block, child, roots, depth + 1);
        }
    }

    let mut block = String::new();
    let _ = writeln!(block, "{START}");
    let _ = writeln!(block, "## 工作空间结构 (swarmx managed)");
    let _ = writeln!(block);
    let _ = writeln!(
        block,
        "下面是本工作空间的项目与它们挂载的依赖源码（树中父子表示\"依赖/归属\"，物理\
         路径见每行）。开发时直接阅读/按需修改这些源码——不要反编译 jar/包、不要凭\
         猜测。改动跨项目的共享库时注意它可能被多处使用。"
    );
    let _ = writeln!(block);

    // The PRIMARY project = (cwd, name, role="project"), implicit root.
    // Its children = roots with parent_id=None && role!="project".
    // Top-level peer projects = roots with parent_id=None && role=="project".
    let _ = writeln!(block, "- 项目 {name} `{cwd}`   (primary)");
    for r in roots
        .iter()
        .filter(|r| r.parent_id.is_none() && r.role != "project")
    {
        emit_node(&mut block, r, roots, 1);
    }
    for r in roots
        .iter()
        .filter(|r| r.parent_id.is_none() && r.role == "project")
    {
        emit_node(&mut block, r, roots, 0);
    }
    let _ = write!(block, "{END}");

    for fname in ["CLAUDE.md", "AGENTS.md"] {
        let path = std::path::Path::new(cwd).join(fname);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        // Is everything OUTSIDE our managed block blank? Then swarmx authored
        // the whole file (created it or it was empty before) — safe to local-
        // exclude so a multi-root workspace doesn't show a perpetual false dirty
        // dot. If the user had their own content (append case), DON'T exclude —
        // we'd hide their real CLAUDE.md.
        let swarmx_only = match (existing.find(START), existing.find(END)) {
            (Some(s), Some(e)) => {
                existing[..s].trim().is_empty() && existing[e + END.len()..].trim().is_empty()
            }
            _ => existing.trim().is_empty(),
        };
        let next = if let (Some(s), Some(e)) = (existing.find(START), existing.find(END)) {
            // replace existing managed block in place
            let end_full = e + END.len();
            format!("{}{}{}", &existing[..s], block, &existing[end_full..])
        } else if existing.trim().is_empty() {
            block.clone()
        } else {
            format!("{}\n\n{}\n", existing.trim_end(), block)
        };
        if let Err(e) = std::fs::write(&path, next) {
            tracing::warn!(?e, file = %path.display(), "failed writing workspace deps context");
        } else {
            tracing::info!(file = %path.display(), roots = roots.len(), "wrote workspace deps context");
            if swarmx_only {
                crate::worktree::ignore_paths_locally(std::path::Path::new(cwd), &[fname]);
            }
        }
    }
}

/// `GET /api/workspaces` — list alive workspaces with their live member
/// counts (alive agents whose `workspace_id` points here). Soft-deleted
/// rows are excluded.
pub async fn list_workspaces_handler(State(state): State<AppState>) -> impl IntoResponse {
    let rows = match state.store.list_workspaces(false).await {
        Ok(rs) => rs,
        Err(e) => {
            tracing::warn!(?e, "list_workspaces failed");
            return Json(Vec::<Workspace>::new());
        }
    };
    // Compute member_count from list_agents instead of per-workspace SQL
    // queries — there are typically <100 agents total, so a single pass
    // beats N+1 SELECTs and keeps the store API smaller.
    let agents = state.store.list_agents().await.unwrap_or_default();
    let mut counts: HashMap<String, i64> = HashMap::new();
    for a in agents {
        if a.killed_at.is_some() {
            continue;
        }
        if let Some(ws_id) = a.workspace_id {
            *counts.entry(ws_id).or_insert(0) += 1;
        }
    }
    // Fetch every attached root in one shot and group by workspace_id (rows
    // come back ordered by created_at ASC, so each group preserves attach
    // order). Same single-pass rationale as member_count above — avoids N+1.
    let mut roots_by_ws: HashMap<String, Vec<WorkspaceRoot>> = HashMap::new();
    for r in state
        .store
        .list_all_workspace_roots()
        .await
        .unwrap_or_default()
    {
        roots_by_ws
            .entry(r.workspace_id)
            .or_default()
            .push(WorkspaceRoot {
                id: r.id,
                path: r.path,
                role: r.role,
                label: r.label,
                parent_id: r.parent_id,
                branch: None, // filled below from branch_map
            });
    }
    // Threads (directions) per workspace. Unlike roots there's no "list all"
    // query; with a handful of workspaces locally, one list_threads each is
    // cheap and keeps the store API minimal. Oldest-first (main leads).
    let mut threads_by_ws: HashMap<String, Vec<ThreadInfo>> = HashMap::new();
    for r in &rows {
        let list = state
            .store
            .list_threads(r.id.clone())
            .await
            .unwrap_or_default();
        threads_by_ws.insert(
            r.id.clone(),
            list.into_iter().map(thread_record_to_info).collect(),
        );
    }
    // Live git branch per path (workspace cwds + every attached root), for the
    // sidebar's branch chips. Batched off the async runtime (git shells out and
    // blocks) and memoized with a short TTL so the frequent workspaces refetch
    // doesn't re-run git every time. Best-effort: on a join error every chip is
    // simply absent.
    let mut paths: Vec<PathBuf> = rows.iter().map(|r| PathBuf::from(&r.cwd)).collect();
    for roots in roots_by_ws.values() {
        paths.extend(roots.iter().map(|rt| PathBuf::from(&rt.path)));
    }
    // Thread cwds too — a worktree direction has its own dir, so each direction
    // gets a live dirty flag (not just the shared workspace cwd).
    for threads in threads_by_ws.values() {
        paths.extend(threads.iter().map(|t| PathBuf::from(&t.cwd)));
    }
    let git_map = tokio::task::spawn_blocking(move || git_status_for_paths(&paths))
        .await
        .unwrap_or_default();
    // Fold the computed branch into the attached roots.
    for roots in roots_by_ws.values_mut() {
        for rt in roots.iter_mut() {
            rt.branch = git_map
                .get(std::path::Path::new(&rt.path))
                .and_then(|g| g.branch.clone());
        }
    }
    // Fold the live dirty flag + branch into each direction (keyed by its cwd).
    // dirty is always live; branch is only filled when the DB row hasn't already
    // recorded one (an isolated worktree keeps its stored branch — its cwd's git
    // HEAD is the same branch anyway, but the stored value is canonical).
    for threads in threads_by_ws.values_mut() {
        for t in threads.iter_mut() {
            if let Some(g) = git_map.get(std::path::Path::new(&t.cwd)) {
                t.dirty = g.dirty;
                if t.branch.is_none() {
                    t.branch = g.branch.clone();
                }
            }
        }
    }
    // ahead/behind: how far each isolated direction's branch has diverged from
    // its workspace's base branch (the main worktree's current branch). Purely
    // local (no fetch). Only worktree directions with a branch != base qualify,
    // so a plain main-only workspace adds ZERO git calls (the common case stays
    // free); the rev-list runs off the async runtime, keyed by thread id.
    let ab_input: Vec<(String, String, String, String)> = rows
        .iter()
        .filter_map(|r| {
            let base = git_map
                .get(std::path::Path::new(&r.cwd))
                .and_then(|g| g.branch.clone())?;
            let threads = threads_by_ws.get(&r.id)?;
            Some(threads.iter().filter_map(move |t| {
                let br = t.branch.as_deref().filter(|b| !b.is_empty())?;
                (t.isolation == "worktree" && br != base)
                    .then(|| (r.cwd.clone(), base.clone(), br.to_string(), t.id.clone()))
            }))
        })
        .flatten()
        .collect();
    if !ab_input.is_empty() {
        let ab_map = tokio::task::spawn_blocking(move || {
            let mut m: std::collections::HashMap<String, (i64, i64)> =
                std::collections::HashMap::new();
            for (cwd, base, branch, tid) in ab_input {
                if let Some(ab) =
                    crate::worktree::ahead_behind(std::path::Path::new(&cwd), &base, &branch)
                {
                    m.insert(tid, ab);
                }
            }
            m
        })
        .await
        .unwrap_or_default();
        for threads in threads_by_ws.values_mut() {
            for t in threads.iter_mut() {
                if let Some(&(ahead, behind)) = ab_map.get(&t.id) {
                    t.ahead = Some(ahead);
                    t.behind = Some(behind);
                }
            }
        }
    }
    let items: Vec<Workspace> = rows
        .into_iter()
        .map(|r| Workspace {
            member_count: counts.get(&r.id).copied().unwrap_or(0),
            roots: roots_by_ws.remove(&r.id).unwrap_or_default(),
            threads: threads_by_ws.remove(&r.id).unwrap_or_default(),
            cwd_branch: git_map
                .get(std::path::Path::new(&r.cwd))
                .and_then(|g| g.branch.clone()),
            id: r.id,
            slug: r.slug,
            name: r.name,
            cwd: r.cwd,
            accent: r.accent,
            created_at: r.created_at,
        })
        .collect();
    Json(items)
}

/// Live git facts for a path: the checked-out branch (for the sidebar chip) and
/// whether the work tree is dirty (uncommitted changes). Both computed at list
/// time, never persisted.
#[derive(Clone, Default)]
pub(crate) struct PathGit {
    branch: Option<String>,
    dirty: bool,
}

/// Live git status for each of `paths`, memoized with a short TTL. Shelling out
/// to git is blocking, so this runs under `spawn_blocking`; the TTL keeps the
/// hot workspaces-list refetch (fired on every thread/agent event) from
/// re-invoking git each time. `branch` is `Some` only for a git work tree on a
/// named branch (else no chip); `dirty` is best-effort (`false` on any error).
fn git_status_for_paths(paths: &[PathBuf]) -> HashMap<PathBuf, PathGit> {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    const TTL: Duration = Duration::from_secs(3);
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (PathGit, Instant)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let mut out: HashMap<PathBuf, PathGit> = HashMap::new();
    for p in paths {
        if out.contains_key(p) {
            continue; // de-dup: a path can appear as both a cwd and a root
        }
        if let Some((g, at)) = cache.lock().get(p) {
            if at.elapsed() < TTL {
                out.insert(p.clone(), g.clone());
                continue;
            }
        }
        // One git call each — both are read-only; `working_dirty` never
        // disturbs an agent mid-edit. status --porcelain is heavier than
        // rev-parse, but the TTL bounds how often it runs per path.
        let g = PathGit {
            branch: crate::worktree::current_branch(p),
            dirty: crate::worktree::working_dirty(p),
        };
        cache.lock().insert(p.clone(), (g.clone(), Instant::now()));
        out.insert(p.clone(), g);
    }
    out
}

// ── threads (directions) ─────────────────────────────────────────────────

/// Ensure `base` is unique among a workspace's ALIVE thread slugs, appending
/// `-2`, `-3`, … on collision. Best-effort: on a list error we return `base`
/// and let the DB's unique index reject a genuine dup.
async fn unique_thread_slug(state: &AppState, workspace_id: &str, base: &str) -> String {
    let existing: std::collections::HashSet<String> = state
        .store
        .list_threads(workspace_id.to_string())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    if !existing.contains(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}-{n}");
        if !existing.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

/// `GET /api/workspaces/:id/threads` — list a workspace's directions
/// (oldest-first; the first entry is the auto-created `main`).
pub async fn list_threads_handler(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
) -> impl IntoResponse {
    let list = state
        .store
        .list_threads(workspace_id)
        .await
        .unwrap_or_default();
    Json(
        list.into_iter()
            .map(thread_record_to_info)
            .collect::<Vec<_>>(),
    )
}

/// `GET /api/workspaces/:id/branches` — local git branches of the workspace
/// cwd, each flagged whether it's already checked out (so the "open existing
/// branch" picker can disable those — a checked-out branch can't be attached to
/// a new worktree). Empty for a non-git workspace. Off the async runtime (git
/// blocks).
pub async fn list_branches_handler(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
) -> impl IntoResponse {
    let ws = match state.store.get_workspace_by_id(workspace_id).await {
        Ok(Some(w)) => w,
        _ => return Json(Vec::<BranchInfo>::new()),
    };
    let cwd = PathBuf::from(ws.cwd);
    let branches = tokio::task::spawn_blocking(move || crate::worktree::list_branches(&cwd))
        .await
        .unwrap_or_default();
    Json(
        branches
            .into_iter()
            .map(|(name, checked_out)| BranchInfo { name, checked_out })
            .collect::<Vec<_>>(),
    )
}

/// `POST /api/workspaces/:id/threads` — open a new direction. Zero-friction:
/// `name` optional; created `shared` + `ready` in the workspace's own cwd. Git
/// isolation is DEFERRED until the direction is named (see
/// `update_thread_handler`) so clicking "+ new direction" never blocks on git.
/// The slug is FIXED at creation and never changes — renaming only moves the
/// display name + git branch, so already-spawned agents' blackboard keys
/// (`{workspace_id}/{slug}/…`) stay valid.
pub async fn create_thread_handler(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<CreateThreadRequest>,
) -> Result<Json<ThreadInfo>, (StatusCode, Json<serde_json::Value>)> {
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
                Json(json!({"error": format!("unknown workspace_id: {workspace_id}")})),
            )
        })?;
    let name = req.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // "Open an existing branch as a direction" — swarmx's worktree-native take
    // on switching branches. When set, the direction's display name/slug default
    // to the branch and the worktree ATTACHES this exact branch instead of
    // creating a fresh one (see `branch` below; `worktree_add` falls back to
    // attach when `-b` fails because the branch exists).
    let open_branch = req
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let display_name = name.or(open_branch);
    // A name (if any) yields a readable, stable slug; otherwise a short random
    // placeholder. Slug is fixed for the thread's life (see doc above).
    let base_slug = match display_name {
        Some(n) => crate::worktree::sanitize_suffix(n),
        None => format!("t-{}", &Uuid::new_v4().to_string()[..6]),
    };
    let slug = unique_thread_slug(&state, &workspace_id, &base_slug).await;
    // A direction named up-front (the "新方向" dialog's name field) must isolate
    // exactly like one the orchestrator names from the first message —
    // otherwise filling that field silently defeats worktree isolation (thread
    // stays `shared`, both directions edit one cwd and clobber each other).
    // Named, non-main → create `preparing` + kick off background `worktree add`;
    // the frontend waits for `ready` before spawning the orchestrator so it
    // lands in the worktree. Unnamed keeps the zero-friction shared/ready path
    // (orchestrator isolates on its first-message naming).
    let will_isolate = display_name.is_some();
    // Worktree binds to the exact existing branch when opening one; otherwise a
    // fresh branch named after the slug.
    let branch = open_branch
        .map(|b| b.to_string())
        .unwrap_or_else(|| slug.clone());
    let rec = state
        .store
        .create_thread(
            NewThread {
                workspace_id: workspace_id.clone(),
                slug,
                name: display_name.map(|s| s.to_string()),
                isolation: "shared".to_string(),
                branch: None,
                cwd: ws.cwd.clone(),
                state: if will_isolate { "preparing" } else { "ready" }.to_string(),
            },
            now_ms(),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    let info = thread_record_to_info(rec);
    if will_isolate {
        // Background git takeover + `worktree add` → flips isolation/cwd/state
        // to worktree/<dir>/ready on success, degrades to shared/ready on any
        // failure (so a non-git project or a git hiccup never blocks the
        // direction — it just stays unisolated, surfaced in the sidebar).
        spawn_thread_worktree(
            state.clone(),
            info.id.clone(),
            workspace_id.clone(),
            ws.cwd.clone(),
            branch,
        );
    }
    publish_thread_changed(&state, &workspace_id, &info.id, "created");
    Ok(Json(info))
}

// ── fusion: multi-model competition fan-out ──────────────────────────────

/// `POST /api/workspaces/:id/fusion` — start a fusion competition. Creates ONE
/// isolated contestant direction per requested label (each named → auto git
/// worktree, so contestants can't clobber each other's files), records a
/// fusion_batches row binding them, and returns it. The same `need` is the
/// directions' display intent; the caller (UI / chat) then sends that need
/// verbatim to each contestant so the comparison is fair.
///
/// Contestants reuse the exact create_thread machinery (slug, preparing→ready
/// worktree takeover) — a fusion contestant is just a named direction that
/// happens to be grouped into a batch. Blackboard isolation between them is
/// already enforced by the per-direction scope (list_blackboard_ops_scoped).
pub async fn create_fusion_handler(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<swarmx_protocol::rest::CreateFusionRequest>,
) -> Result<Json<swarmx_protocol::rest::FusionBatch>, (StatusCode, Json<serde_json::Value>)> {
    let need = req.need.trim();
    if need.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "need must not be empty"})),
        ));
    }
    // Cost guard: a fusion runs N real CLIs at once. Cap N so a typo can't
    // fan out 50 contestants and exhaust the user's plan/rate limits.
    let mut labels: Vec<String> = req
        .labels
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // Auto-implement panel: label → "cli[:model]". In autopilot the server picks
    // the panel (and derives the labels) when the caller gave none — the one-click
    // novice path. Otherwise it's exactly what the caller sent.
    let mut panel = req.panel.clone().unwrap_or_default();
    if req.autopilot && panel.is_empty() {
        panel = autopilot_panel(&state);
        if panel.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "autopilot 需要至少一个可用引擎：请在设置里配置 Comate License，或安装并登录一个 CLI"})),
            ));
        }
        if labels.is_empty() {
            labels = panel.keys().cloned().collect();
            labels.sort(); // deterministic contestant order
        }
    }

    if labels.len() < 2 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "a fusion needs at least 2 contestants"})),
        ));
    }
    if labels.len() > 4 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at most 4 contestants (cost guard — each runs a real CLI)"})),
        ));
    }
    let ws = state
        .store
        .get_workspace_by_id(workspace_id.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown workspace_id: {workspace_id}")})),
            )
        })?;

    // Short batch slug from the need (fs-friendly), e.g. "implement JWT login"
    // → "implement-jwt-login". Contestant slugs are "<batch>-<label>".
    let base_batch_slug = crate::worktree::sanitize_suffix(need);
    let base_batch_slug = if base_batch_slug.is_empty() {
        "fusion".to_string()
    } else {
        base_batch_slug.chars().take(40).collect::<String>()
    };

    // Auto-implement panel resolved above (autopilot fills it; else the caller's).
    let valid_clis = ["claude", "codex", "opencode", "reasonix", "zulu", "kimi"];

    // Create one isolated contestant direction per label.
    let mut contestant_ids: Vec<String> = Vec::with_capacity(labels.len());
    // Agent ids of the auto-implement contestants — the autopilot autochain
    // watches these to know when to enter the judge stage.
    let mut panel_agent_ids: Vec<String> = Vec::new();
    for label in &labels {
        let label_slug = crate::worktree::sanitize_suffix(label);
        let base_slug = format!("{base_batch_slug}-{label_slug}");
        let slug = unique_thread_slug(&state, &workspace_id, &base_slug).await;
        let name = format!("{need} · {label}");
        let rec = state
            .store
            .create_thread(
                NewThread {
                    workspace_id: workspace_id.clone(),
                    slug: slug.clone(),
                    name: Some(name),
                    isolation: "shared".to_string(),
                    branch: None,
                    cwd: ws.cwd.clone(),
                    state: "preparing".to_string(),
                },
                now_ms(),
            )
            .await
            .map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            })?;

        // Does this label want an auto-implement agent? Parse "cli" or
        // "cli:model" (e.g. "zulu:Deepseek V4 Pro" to race different zulu models
        // — one license, N models). Only the cli part is case-normalized.
        let (panel_cli, panel_model) = match panel.get(label).map(|s| s.trim()) {
            Some(v) if !v.is_empty() => {
                let (c, m) = match v.split_once(':') {
                    Some((c, m)) => (c.trim().to_lowercase(), Some(m.trim().to_string())),
                    None => (v.to_lowercase(), None),
                };
                if valid_clis.contains(&c.as_str()) {
                    (Some(c), m)
                } else {
                    (None, None)
                }
            }
            _ => (None, None),
        };

        if let Some(cli) = panel_cli {
            // Auto-implement: synchronously isolate THIS contestant's worktree
            // (we need the dir to spawn the agent in it) WITHOUT going through
            // spawn_thread_worktree — whose success path reroots an orchestrator
            // that would kill the contestant agent we're about to spawn. Then
            // spawn the CLI agent with a prompt to implement `need`. Best-effort:
            // an isolation/spawn failure degrades the contestant to an empty
            // user-driven worktree rather than failing the whole batch.
            match spawn_panel_contestant(
                &state,
                &workspace_id,
                &rec.id,
                &slug,
                &ws.cwd,
                &cli,
                panel_model.as_deref(),
                need,
            )
            .await
            {
                Ok(agent_id) => panel_agent_ids.push(agent_id),
                Err((_, msg)) => {
                    tracing::warn!(label = %label, cli = %cli, msg = %msg, "fusion panel: auto-implement spawn failed; contestant left user-driven");
                    // Fall back to the normal background isolation so the user
                    // can still drive it by hand.
                    spawn_thread_worktree(
                        state.clone(),
                        rec.id.clone(),
                        workspace_id.clone(),
                        ws.cwd.clone(),
                        slug.clone(),
                    );
                }
            }
        } else {
            // User-driven contestant: kick the background worktree takeover
            // (same as a named direction). The user drives it from the chat.
            spawn_thread_worktree(
                state.clone(),
                rec.id.clone(),
                workspace_id.clone(),
                ws.cwd.clone(),
                slug.clone(),
            );
        }
        publish_thread_changed(&state, &workspace_id, &rec.id, "created");
        contestant_ids.push(rec.id);
    }

    // Record the batch binding the contestants.
    let batch_slug = unique_fusion_slug(&state, &workspace_id, &base_batch_slug).await;
    let batch = state
        .store
        .create_fusion_batch(
            swarmx_storage::NewFusionBatch {
                workspace_id: workspace_id.clone(),
                slug: batch_slug,
                need: need.to_string(),
                contestant_thread_ids: contestant_ids,
                check_cmd: req.check_cmd.clone(),
            },
            now_ms(),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Autopilot: chain straight through to the judge stage once the contestants
    // settle — no human clicks. Gate on "are there contestants to judge", NOT
    // "did at least one agent spawn": when every panelist's agent fails to spawn
    // (all engines rate-limited/logged-out) each still leaves a user-driven
    // contestant direction, and the autochain treats agent-less contestants as
    // settled → judge. Gating on panel_agent_ids left those batches stuck in
    // 'running' with no task ever advancing them.
    if req.autopilot && !batch.contestant_thread_ids.is_empty() {
        spawn_fusion_autochain(
            state.clone(),
            workspace_id.clone(),
            batch.id.clone(),
            batch.contestant_thread_ids.clone(),
            ws.cwd.clone(),
        );
    }

    Ok(Json(fusion_record_to_wire(batch)))
}

/// Auto-select a fusion panel for autopilot when the caller gave none. Prefers
/// ≥2 distinct ready real CLIs (a genuine cross-engine race); else races three
/// zulu models under one license; else empty (the caller surfaces a clear error).
/// Returns label → "cli[:model]".
fn autopilot_panel(state: &AppState) -> std::collections::HashMap<String, String> {
    use std::collections::HashSet;
    let usable: HashSet<String> = crate::engine_probe::cached_results()
        .into_iter()
        .filter(|r| matches!(r.state, crate::engine_probe::ProbeState::Usable))
        .map(|r| r.engine)
        .collect();
    // Ready = probed usable, or (no probe yet) the binary is at least installed.
    let ready = |id: &str| -> bool {
        usable.contains(id)
            || state
                .plugins
                .get(id)
                .map(|p| crate::runtime_path::resolve_executable(&p.binary).is_some())
                .unwrap_or(false)
    };
    let clis: Vec<String> = ["claude", "codex", "opencode", "reasonix", "kimi"]
        .into_iter()
        .filter(|c| ready(c))
        .take(3)
        .map(String::from)
        .collect();
    if clis.len() >= 2 {
        return clis.into_iter().map(|c| (c.clone(), c)).collect();
    }
    // One license → three models. Needs a configured license to actually run.
    if ready("zulu") && !crate::comate::load_license().is_empty() {
        return ["Deepseek V4 Pro", "GLM-5.2", "Kimi-K2.6"]
            .into_iter()
            .map(|m| {
                let label: String = crate::worktree::sanitize_suffix(m).chars().take(20).collect();
                (label, format!("zulu:{m}"))
            })
            .collect();
    }
    std::collections::HashMap::new()
}

/// Env-tunable ceiling for autopilot to wait for contestants to implement.
fn autochain_impl_timeout() -> std::time::Duration {
    std::env::var("SWARMX_FUSION_IMPL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or_else(|| std::time::Duration::from_secs(20 * 60))
}

/// Autopilot orchestration: watch the auto-implement contestants and, once
/// they've all settled, enter the synthesize judge stage — whose watchdog lands
/// the merge. Zero human clicks. "Settled" is COMMITTED work (the
/// engine-independent done signal — `last_activity_at` is unreliable: zulu isn't
/// transcript-tailed, codex intermittently), OR the agent is terminal, OR idle
/// past the grace. If contestants never settle by the timeout, chain anyway
/// (weak/empty ones lose under the judge). No-op if the batch left 'running'.
fn spawn_fusion_autochain(
    state: AppState,
    workspace_id: String,
    batch_id: String,
    contestant_thread_ids: Vec<String>,
    base_cwd: String,
) {
    tokio::spawn(async move {
        const POLL: std::time::Duration = std::time::Duration::from_secs(5);
        let max = autochain_impl_timeout();
        let start = std::time::Instant::now();
        // Base branch (what contestants forked from) — computed once.
        let base_branch = {
            let c = base_cwd.clone();
            tokio::task::spawn_blocking(move || crate::worktree::current_branch(std::path::Path::new(&c)))
                .await
                .ok()
                .flatten()
        };
        loop {
            tokio::time::sleep(POLL).await;
            if current_batch_status(&state, &workspace_id, &batch_id).await.as_deref() != Some("running") {
                return;
            }
            let timed_out = start.elapsed() >= max;
            if !timed_out
                && !contestants_settled(
                    &state,
                    &contestant_thread_ids,
                    base_branch.as_deref(),
                    &base_cwd,
                )
                .await
            {
                continue;
            }
            let Ok(ws) = require_workspace(&state, &workspace_id).await else {
                return;
            };
            let Some(batch) = state
                .store
                .list_fusion_batches(workspace_id.clone())
                .await
                .ok()
                .and_then(|bs| bs.into_iter().find(|b| b.id == batch_id))
            else {
                return;
            };
            if batch.status != "running" {
                return;
            }
            tracing::info!(batch = %batch_id, timed_out, "fusion autopilot: contestants settled → entering synthesize judge stage");
            if let Err((_, e)) = enter_judge_stage(state.clone(), ws, batch, true, true).await {
                tracing::warn!(batch = %batch_id, err = ?e, "fusion autopilot: enter_judge_stage failed");
            }
            return;
        }
    });
}

/// True when EVERY contestant has settled. Per contestant, settled = its branch
/// has committed work (`diff_summary` non-empty — the reliable, engine-agnostic
/// "done" signal), OR its agent is terminal (killed/exited), OR idle past the
/// grace, OR its agent row / thread is gone. Any contestant that is none of these
/// is still working → not settled.
async fn contestants_settled(
    state: &AppState,
    thread_ids: &[String],
    base_branch: Option<&str>,
    base_cwd: &str,
) -> bool {
    if thread_ids.is_empty() {
        return true;
    }
    let agents = state.store.list_agents().await.unwrap_or_default();
    for tid in thread_ids {
        // Terminal by the contestant's agent row (idle-time is NOT trusted — it
        // can't tell "done" from "mid-implementation"; a working contestant would
        // be prematurely judged out).
        let agent = agents
            .iter()
            .find(|a| a.thread_id.as_deref() == Some(tid) && a.role == "fusion-contestant");
        let terminal = match agent {
            None => true,
            Some(a) => a.killed_at.is_some() || a.shim_exit_at.is_some(),
        };
        if terminal {
            continue;
        }
        // Still-live agent: has it COMMITTED work yet? That's the done signal.
        let committed = match (base_branch, state.store.get_thread(tid.clone()).await) {
            (Some(base), Ok(Some(th))) if th.isolation == "worktree" => {
                match th.branch.clone().filter(|b| !b.is_empty()) {
                    Some(branch) => {
                        let repo = base_cwd.to_string();
                        let base = base.to_string();
                        tokio::task::spawn_blocking(move || {
                            crate::worktree::diff_summary(std::path::Path::new(&repo), &base, &branch)
                        })
                        .await
                        .map(|f| !f.is_empty())
                        .unwrap_or(false)
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if !committed {
            return false; // this contestant is still working
        }
    }
    true
}

/// `GET /api/workspaces/:id/fusion` — list alive fusion batches (newest first).
pub async fn list_fusion_handler(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<Vec<swarmx_protocol::rest::FusionBatch>>, (StatusCode, Json<serde_json::Value>)> {
    let batches = state
        .store
        .list_fusion_batches(workspace_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    Ok(Json(batches.into_iter().map(fusion_record_to_wire).collect()))
}

/// `POST /api/workspaces/:id/fusion/:bid/judge` — enter the judge stage. Creates
/// ONE privileged judge direction (its own isolated worktree off the base) and
/// gathers every contestant's diff bundle (branch + changed files vs base) for
/// review, then flips the batch to `judging`. The judge is the deliberate
/// inverse of contestant isolation: it cross-reads all contestants' changesets
/// (via each one's branch diff) so it can compare and synthesize — the
/// contestants never saw each other, but the judge sees them all.
pub async fn judge_fusion_handler(
    State(state): State<AppState>,
    Path((workspace_id, batch_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<swarmx_protocol::rest::FusionJudgeResponse>, (StatusCode, Json<serde_json::Value>)> {
    // `?auto=true` spawns the judge agent that reads diffs, picks/synthesizes, and
    // decides itself; `?synthesize=true` (auto only) makes it hand-write a
    // combined-best implementation and merge THAT. Both default false (manual
    // flow: judge direction + human decide, unchanged).
    let auto = params
        .get("auto")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let synthesize = params
        .get("synthesize")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let ws = require_workspace(&state, &workspace_id).await?;
    let batch = state
        .store
        .list_fusion_batches(workspace_id.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .into_iter()
        .find(|b| b.id == batch_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown fusion batch: {batch_id}")})),
            )
        })?;
    enter_judge_stage(state, ws, batch, auto, synthesize).await.map(Json)
}

/// The judge-stage core, callable by the HTTP handler AND the autopilot autochain
/// (in-process, no self-HTTP). Creates the privileged judge direction, gathers
/// every contestant's diff bundle (+ runs the objective gate when the batch
/// carries a check_cmd, auto and manual alike), flips the batch to `judging`,
/// and in auto mode spawns the judge agent plus its watchdog. Takes an owned
/// `AppState` so the body's `&state`/`state.clone()` usages are untouched.
async fn enter_judge_stage(
    state: AppState,
    ws: swarmx_storage::WorkspaceRecord,
    batch: swarmx_storage::FusionBatchRecord,
    auto: bool,
    synthesize: bool,
) -> Result<swarmx_protocol::rest::FusionJudgeResponse, (StatusCode, Json<serde_json::Value>)> {
    let workspace_id = ws.id.clone();
    let batch_id = batch.id.clone();

    // Idempotency (cheap pre-check): only a 'running' batch may enter the judge
    // stage. A batch already judging/decided — a double-click, autopilot racing a
    // manual click, or a replay of a done batch — is rejected here before any
    // judge worktree/diff work, so we don't build an orphan judge direction. The
    // atomic guard is the CAS at the flip point below.
    if batch.status != "running" {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "fusion batch is '{}', not 'running'; judge stage already entered",
                    batch.status
                )
            })),
        ));
    }

    // Base = the branch checked out at the workspace cwd (what contestants
    // forked from). Computed once; contestant diffs are taken vs this.
    let cwd = std::path::PathBuf::from(&ws.cwd);
    let base = {
        let cwd = cwd.clone();
        tokio::task::spawn_blocking(move || crate::worktree::current_branch(&cwd))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
    };

    // The objective gate command, if this batch carries one. Runs in BOTH
    // modes: the auto-judge consumes the result in its prompt, and the manual
    // flow shows pass/fail + output tail on the contestant cards — previously
    // manual judging silently ignored a check_cmd the create form accepts for
    // both modes (the Fusion form offers the input unconditionally).
    let check_cmd = batch.check_cmd.clone().filter(|c| !c.trim().is_empty());

    // Gather each contestant's diff bundle.
    let mut contestants: Vec<swarmx_protocol::rest::FusionContestantDiff> = Vec::new();
    for tid in &batch.contestant_thread_ids {
        let th = match state.store.get_thread(tid.clone()).await {
            Ok(Some(t)) => t,
            _ => continue, // contestant deleted mid-competition — skip, stay honest
        };
        let branch = th.branch.clone().filter(|b| !b.is_empty());
        let degraded = th.isolation != "worktree" || branch.is_none();
        let files = match (&base, &branch) {
            (Some(b), Some(f)) => {
                let cwd = cwd.clone();
                let b = b.clone();
                let f = f.clone();
                tokio::task::spawn_blocking(move || crate::worktree::diff_summary(&cwd, &b, &f))
                    .await
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        // OBJECTIVE GATE: with a check_cmd, RUN the check in this contestant's
        // worktree before any deliberation — auto AND manual mode alike. This
        // catches the "looks correct but fails at runtime" class a pure-diff
        // judge misses. The contestant's worktree dir is derived the same way
        // isolation does.
        let (check_passed, check_output) = if check_cmd.is_some() {
            match (&th.isolation == "worktree", th.branch.as_deref()) {
                (true, Some(br)) if !br.is_empty() => {
                    let dir = crate::worktree::worktree_dest(&cwd, br)
                        .to_string_lossy()
                        .into_owned();
                    let (passed, out) =
                        run_contestant_check(&dir, check_cmd.as_deref().unwrap()).await;
                    (Some(passed), Some(out))
                }
                // Degraded contestant (no isolated worktree) — can't run an
                // isolated check; report as failed gate so the judge doesn't
                // treat unverifiable work as passing.
                _ => (
                    Some(false),
                    Some("no isolated worktree — check could not be run".to_string()),
                ),
            }
        } else {
            (None, None)
        };
        contestants.push(swarmx_protocol::rest::FusionContestantDiff {
            thread_id: th.id,
            slug: th.slug,
            name: th.name,
            branch,
            files,
            degraded,
            check_passed,
            check_output,
        });
    }

    // Create the judge direction (named → isolated worktree off base, same
    // machinery as a contestant; it just gets cross-read privileges via this
    // response rather than any special isolation).
    let judge_base_slug = format!("{}-judge", batch.slug);
    let judge_slug = unique_thread_slug(&state, &workspace_id, &judge_base_slug).await;
    let judge_rec = state
        .store
        .create_thread(
            NewThread {
                workspace_id: workspace_id.clone(),
                slug: judge_slug.clone(),
                name: Some(format!("{} · judge", batch.need)),
                isolation: "shared".to_string(),
                branch: None,
                cwd: ws.cwd.clone(),
                state: "preparing".to_string(),
            },
            now_ms(),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    // The judge worktree dir, populated synchronously ONLY in auto mode (we
    // need it on hand to spawn the judge agent there). In manual mode we keep
    // the original fire-and-forget isolation so the existing flow is untouched.
    let mut judge_cwd = ws.cwd.clone();
    if auto {
        // Synchronous isolation: we must know the judge worktree dir before we
        // can spawn the agent in it, and — critically — we must NOT go through
        // spawn_thread_worktree, whose success path runs reroot_thread_orchestrator
        // (it kills every agent on the thread and respawns an `init` orchestrator),
        // which would tear down the judge agent we're about to spawn.
        let cwd_for_git = ws.cwd.clone();
        let branch_for_git = judge_slug.clone();
        let git_result = tokio::task::spawn_blocking(move || {
            crate::worktree::isolate_into_worktree(
                std::path::Path::new(&cwd_for_git),
                &branch_for_git,
            )
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
        match git_result {
            Ok(dest) => {
                let dest_str = dest.to_string_lossy().into_owned();
                let _ = state
                    .store
                    .update_thread(
                        judge_rec.id.clone(),
                        None,
                        None,
                        Some("worktree".to_string()),
                        Some(judge_slug.clone()),
                        Some(dest_str.clone()),
                        Some("ready".to_string()),
                    )
                    .await;
                write_deps_context_into_dir(&state, &workspace_id, &dest_str).await;
                judge_cwd = dest_str;
                publish_thread_changed(&state, &workspace_id, &judge_rec.id, "isolated");
            }
            Err(e) => {
                // Isolation failed — degrade to shared (judge runs in main cwd),
                // stay honest. The judge agent can still diff contestants via
                // their branches; it just isn't itself in a private worktree.
                tracing::warn!(?e, thread = %judge_rec.id, "auto-judge: git isolation failed; judge stays shared");
                degrade_thread_to_shared(&state, &judge_rec.id).await;
                publish_thread_changed(&state, &workspace_id, &judge_rec.id, "updated");
            }
        }
    } else {
        spawn_thread_worktree(
            state.clone(),
            judge_rec.id.clone(),
            workspace_id.clone(),
            ws.cwd.clone(),
            judge_slug,
        );
        publish_thread_changed(&state, &workspace_id, &judge_rec.id, "created");
    }

    // Atomically CLAIM the judge stage: running→judging only if still running.
    // This is the single race gate. A concurrent entrant (double-click, or the
    // autopilot autochain racing a manual judge click) gets 0 rows and aborts
    // here — so we never spawn a duplicate judge agent + watchdog, and a batch
    // that a peer already decided can never be flipped back into 'judging'
    // (the exact defect that could strand a done batch in judging forever).
    let claimed = state
        .store
        .transition_fusion_status(batch.id.clone(), "running".to_string(), "judging".to_string())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    if claimed == 0 {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "another judge already entered this batch's judge stage" })),
        ));
    }
    // Bind the judge direction (status is already 'judging' from the CAS above).
    state
        .store
        .set_fusion_judge(batch.id.clone(), judge_rec.id.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Re-read so the returned batch reflects the judge + 'judging' status.
    let updated = state
        .store
        .list_fusion_batches(workspace_id.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .into_iter()
        .find(|b| b.id == batch.id)
        .map(fusion_record_to_wire)
        .ok_or_else(|| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "batch vanished after update"})))
        })?;

    // Auto mode: spawn the real judge CLI agent in the judge worktree, inject a
    // Chinese system prompt that tells it to diff every contestant's branch,
    // pick a winner, and curl the decide endpoint to close the loop. Best-effort:
    // a spawn failure leaves the manual flow available, so we report the agent id
    // when it spawned and None otherwise rather than failing the whole request.
    let judge_agent_id = if auto {
        match spawn_auto_judge(
            &state,
            &workspace_id,
            &batch_id,
            &judge_rec.id,
            &judge_cwd,
            &batch.need,
            base.as_deref(),
            &contestants,
            synthesize,
        )
        .await
        {
            Ok(id) => Some(id),
            Err((_, msg)) => {
                tracing::warn!(batch = %batch_id, msg = %msg, "auto-judge: spawning judge agent failed");
                None
            }
        }
    } else {
        None
    };

    // Guarantee the batch NEVER stalls in 'judging'. The auto-judge's only path to
    // decide is the judge LLM agent's own curl; if it stops early (crash / turn
    // limit / forgot the curl / CLI died) the batch would be stuck forever. This
    // watchdog observes the judge agent's lifecycle and, when it's gone/idle with
    // the batch still 'judging', runs a DETERMINISTIC fallback decide (synth →
    // merge the judge's captured work; pick+check → the gate winner; otherwise →
    // 'needs_decision' so the UI offers manual pick). Spawned even when the judge
    // agent failed to spawn (that also leaves the batch stuck). Idempotent with
    // the judge's own successful decide via the storage CAS. Auto mode only.
    if auto {
        spawn_judge_watchdog(
            state.clone(),
            workspace_id.clone(),
            batch_id.clone(),
            judge_rec.id.clone(),
            judge_agent_id.clone(),
            synthesize,
            check_cmd.clone(),
            base.clone(),
            ws.cwd.clone(),
            contestants.clone(),
        );
    }

    Ok(swarmx_protocol::rest::FusionJudgeResponse {
        batch: updated,
        judge_thread_id: judge_rec.id,
        base,
        contestants,
        judge_agent_id,
    })
}

/// Auto-implement one panel contestant: synchronously isolate its worktree,
/// then spawn a real CLI agent inside it to implement `need` autonomously. This
/// is the OpenRouter-fusion-style full-auto panel — instead of the user driving
/// each contestant from the chat, every panel contestant is its own independent
/// model implementing the SAME need in isolation. Mirrors the auto-judge's
/// synchronous-isolation pattern (must NOT use spawn_thread_worktree, whose
/// success path reroots an orchestrator that would kill the agent we spawn).
async fn spawn_panel_contestant(
    state: &AppState,
    workspace_id: &str,
    thread_id: &str,
    slug: &str,
    ws_cwd: &str,
    cli: &str,
    model: Option<&str>,
    need: &str,
) -> Result<String, (StatusCode, String)> {
    // Synchronous isolation: we need the worktree dir on hand before spawning.
    let cwd_for_git = ws_cwd.to_string();
    let branch_for_git = slug.to_string();
    let dest = tokio::task::spawn_blocking(move || {
        crate::worktree::isolate_into_worktree(
            std::path::Path::new(&cwd_for_git),
            &branch_for_git,
        )
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let dest_str = dest.to_string_lossy().into_owned();

    // Mark the thread as isolated (worktree + branch + ready), same as the
    // auto-judge isolation success path.
    let _ = state
        .store
        .update_thread(
            thread_id.to_string(),
            None,
            None,
            Some("worktree".to_string()),
            Some(slug.to_string()),
            Some(dest_str.clone()),
            Some("ready".to_string()),
        )
        .await;
    write_deps_context_into_dir(state, workspace_id, &dest_str).await;
    publish_thread_changed(state, workspace_id, thread_id, "isolated");

    // The implement prompt: do the need, commit, then stop. Kept deliberately
    // minimal — the contestant must NOT talk to other contestants (it can't see
    // them) and must NOT touch the judge; it just implements and commits.
    let system_prompt = format!(
        "你是一次 fusion 多模型竞赛中的一名『参赛选手』。你和其它选手拿到的是**同一个需求**，\
         各自在自己隔离的 git worktree 里独立实现，互相看不到对方的代码。\n\n\
         ## 你的需求\n{need}\n\n\
         ## 你要做的\n\
         1. 你当前的工作目录就是你专属的隔离 worktree，放手实现这个需求，把代码写进合适的文件。\n\
         2. 追求正确、完整、可读：真正满足需求，处理好边界情况，别留明显 bug。\n\
         3. 实现完成后，用 git 把你的改动**提交**（`git add -A && git commit -m \"...\"`）。\n\
         4. 提交后用 swarm_send_message 给 `user` 发一句话简述你的实现思路，然后停止。\n\n\
         注意：只实现你自己的版本，别去找别的选手，也别管裁判——评比是后续独立的环节。",
    );

    let layout = WorkspaceLayout::Shared {
        dir: std::path::PathBuf::from(dest_str),
    };
    let spawn_ms = now_ms();
    let out = spawn_with_bookkeeping(
        state,
        cli,
        Some("fusion-contestant".to_string()),
        model.map(str::to_string),
        None,
        layout,
        workspace_id.to_string(),
        None,
        Some(thread_id.to_string()),
    )
    .await?;
    spawn_bootstrap_inject(
        state.registry.clone(),
        out.lifecycle_rx.resubscribe(),
        out.agent_id.clone(),
        system_prompt,
        BootstrapCtx {
            source: "worker",
            spell: String::new(),
            role_keys: Vec::new(),
        },
        Vec::new(),
        state.swarm.clone(),
        state.plugins.clone(),
        spawn_ms,
        state.server_url.clone(),
    );
    Ok(out.agent_id)
}

/// Run the batch's objective check command inside one contestant's worktree
/// and return (passed, output_tail). This is the OBJECTIVE gate that a pure-diff
/// judge lacks: code that LOOKS correct but fails at runtime (a `>` that should
/// be `>=`, a deadlock) is caught by RUNNING it, not reading it. Best-effort: a
/// missing worktree / spawn failure is reported as a failed check with the error
/// as output, so a contestant is never silently passed. Output is tail-truncated
/// to keep the judge prompt bounded.
async fn run_contestant_check(worktree_dir: &str, check_cmd: &str) -> (bool, String) {
    run_contestant_check_with_timeout(worktree_dir, check_cmd, CHECK_TIMEOUT).await
}

/// Hard ceiling for one contestant check run. Test/build suites can be slow;
/// this only bounds a pathological hang — before it, a check command that
/// never exits (e.g. `npm test` accidentally starting a dev server) wedged
/// the judge FOREVER on `.output()`. Same value and reasoning as verify.rs's
/// VERIFY_TIMEOUT.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Test seam: same as [`run_contestant_check`] with a caller-chosen timeout,
/// so the hung-check path is exercised in milliseconds.
async fn run_contestant_check_with_timeout(
    worktree_dir: &str,
    check_cmd: &str,
    timeout: std::time::Duration,
) -> (bool, String) {
    let dir = worktree_dir.to_string();
    let cmd = check_cmd.to_string();
    let result =
        tokio::task::spawn_blocking(move || run_check_timeboxed(&cmd, &dir, timeout)).await;

    match result {
        Ok(Ok(out)) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            // Tail-truncate: the failure usually surfaces at the end (assertion,
            // traceback, compiler error summary).
            let tail = tail_chars(&combined, 1500);
            (out.status.success(), tail)
        }
        Ok(Err(msg)) => (false, msg),
        Err(e) => (false, format!("check task panicked/cancelled: {e}")),
    }
}

/// Blocking core of the contestant check: spawn the shell, wait with a hard
/// timeout (the `worktree.rs::git()` channel pattern), and on timeout kill the
/// whole process tree so a hung check leaves no orphans holding the worktree.
/// A timeout is a FAILED check with the evidence in the message (fail-loud —
/// the judge must never treat an unverifiable contestant as passing).
fn run_check_timeboxed(
    cmd: &str,
    dir: &str,
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    let mut c = crate::runtime_path::shell_command(cmd);
    c.current_dir(dir);
    // Own process group so a timeout can killpg the whole forked tree
    // (`sh -c "npm test"` forks grandchildren; killing just the shell strands
    // them). Same pattern as verify.rs's exec_timeboxed.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let child = c
        .spawn()
        .map_err(|e| format!("failed to run check command: {e}"))?;
    let pid = child.id(); // == pgid on unix (process_group(0))
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        // wait_with_output drains both pipes (so a full pipe can't deadlock
        // the child) and reaps it.
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => {
            let _ = waiter.join();
            Ok(out)
        }
        Ok(Err(e)) => {
            let _ = waiter.join();
            Err(format!("check wait failed: {e}"))
        }
        Err(_) => {
            kill_check_tree(pid);
            // Where a real tree-kill exists the child is already dying, so
            // join reaps it promptly. On the compile-only fallback (neither
            // unix nor windows) no kill exists — detach rather than block the
            // judge on the runaway child.
            #[cfg(any(unix, windows))]
            let _ = waiter.join();
            #[cfg(not(any(unix, windows)))]
            drop(waiter);
            Err(format!(
                "check command timed out after {timeout:?}; process tree killed"
            ))
        }
    }
}

/// Best-effort tree kill for a timed-out contestant check.
#[cfg(unix)]
fn kill_check_tree(pid: u32) {
    // SAFETY: killpg on a pgid we created via process_group(0) above.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// Windows has no killpg; `taskkill /T` kills the shell's whole child tree.
#[cfg(windows)]
fn kill_check_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output();
}

/// Compile-only fallback: std has no portable kill-by-pid. The judge is still
/// unblocked (the caller detaches the waiter instead of joining it) and the
/// check still fails loud; the runaway child finishes on its own.
#[cfg(not(any(unix, windows)))]
fn kill_check_tree(_pid: u32) {}

/// Keep the last `max` chars of `s`, prefixing an ellipsis when truncated.
fn tail_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - max..].iter().collect();
    format!("…(truncated)…\n{tail}")
}

/// Spawn a one-shot CLI agent inside the judge direction's worktree to perform
/// the verdict autonomously: read every contestant's diff (`git diff base...branch`),
/// pick a winner on quality, and POST the decide endpoint via curl to land it.
/// Mirrors `spawn_merge_resolver`'s spawn_with_bookkeeping + spawn_bootstrap_inject
/// two-step. The agent lands on the judge thread_id so it operates in the judge
/// worktree, with cross-read access to all contestant branches (they all live in
/// the same underlying git repo).
#[allow(clippy::too_many_arguments)]
async fn spawn_auto_judge(
    state: &AppState,
    workspace_id: &str,
    batch_id: &str,
    judge_thread_id: &str,
    judge_cwd: &str,
    need: &str,
    base: Option<&str>,
    contestants: &[swarmx_protocol::rest::FusionContestantDiff],
    synthesize: bool,
) -> Result<String, (StatusCode, String)> {
    let cli = resolver_cli(state, workspace_id).await;
    let n = contestants.len();
    let base_branch = base.unwrap_or("(未知，请用 git branch 自行确认主线)");

    // Build a per-contestant briefing: slug + branch + the files it touched, so
    // the judge knows exactly which branches to diff and what to expect.
    let mut roster = String::new();
    for (i, c) in contestants.iter().enumerate() {
        let branch = c.branch.as_deref().unwrap_or("(无独立分支——隔离降级，无法 diff)");
        let files = if c.files.is_empty() {
            "(没有改动任何文件)".to_string()
        } else {
            c.files.join("、")
        };
        let degraded = if c.degraded { "（已降级，谨慎评估）" } else { "" };
        // Objective gate result, when a check was run. This is the single most
        // important signal — code that FAILS the check is objectively broken no
        // matter how clean its diff reads.
        let check_line = match (c.check_passed, c.check_output.as_deref()) {
            (Some(true), _) => "\n   ✅ 客观检查：通过（已在该选手 worktree 真实跑过 check 命令）".to_string(),
            (Some(false), Some(out)) => format!(
                "\n   ❌ 客观检查：失败（已在该选手 worktree 真实跑过 check 命令）——这是硬证据，\
                 无论 diff 读起来多漂亮，失败就是失败。检查输出尾部：\n   ```\n{out}\n   ```",
            ),
            (Some(false), None) => "\n   ❌ 客观检查：失败".to_string(),
            (None, _) => String::new(),
        };
        roster.push_str(&format!(
            "{}. 选手 slug=`{}`，thread_id=`{}`，分支=`{}`{}\n   改动文件：{}{}\n",
            i + 1,
            c.slug,
            c.thread_id,
            branch,
            degraded,
            files,
            check_line,
        ));
    }

    // Whether any objective check ran at all (drives the extra prompt rule).
    let any_check_ran = contestants.iter().any(|c| c.check_passed.is_some());
    let check_rule = if any_check_ran {
        "\n\n## 客观检查优先（最高优先级，硬规则）\n\
         上面每位选手都已在各自的 worktree 里真实跑过一条 check 命令（编译/测试）。\
         这是比你读 diff 更可靠的证据：**任何客观检查失败的选手，一律不得当选**，\
         哪怕它的代码看起来最优雅。只在客观检查通过的选手里挑赢家；\
         若只有一个通过，它就是赢家；若全部失败，选其中失败最轻、最接近正确的那个并在留言里如实说明。\n"
    } else {
        ""
    };

    let server_url = state.server_url.clone();
    let decide_url = format!(
        "{}/api/workspaces/{}/fusion/{}/decide",
        server_url.trim_end_matches('/'),
        workspace_id,
        batch_id,
    );

    let analysis_key = format!("{}.judge-analysis", batch_id);
    let system_prompt = if synthesize {
        format!(
        "你是本次 fusion 竞赛的『综合者』。同一个需求被 {n} 个互相隔离的选手各自独立实现，\
         你的任务不是挑一个赢家，而是**博采众长、亲手综合出一份最优实现**。\n\n\
         ## 本次需求\n{need}\n\n## 主线（base）分支\n`{base_branch}`\n\n## 参赛选手\n{roster}\n\
         ## 你的任务（务必逐步执行）\n\
         1. 你当前的工作目录是一个独立的 git worktree（从主线拉出的分支）。所有选手分支在同一仓库可见。\n\
         2. 逐个读每位选手的改动（`git diff {base_branch}...<选手分支名>`），看清各家优点、取舍、独特亮点、共同盲区。\n\
         3. **在你当前的工作目录里，动手写出一份综合各家最优的实现**：正确性优先，吸收每家最好的部分，补上大家都漏的点。写完 `git add -A && git commit`。\n\
         4. 提交后，通过 curl 调用 decide 端点，用**你自己的 thread_id `{judge_thread_id}`** 作为 winner，把这份综合版合并进主线：\n\
         ```\n\
         curl -X POST {decide_url} -H 'content-type: application/json' -d '{{\"winner_thread_id\":\"{judge_thread_id}\"}}'\n\
         ```\n\
         5. curl 成功后用 swarm_send_message 给 `user` 说明你综合了各家哪些优点、补了什么。然后停止。\n\n\
         注意：这是综合模式——你要亲手写出并提交综合实现，然后用你自己的 thread_id 落地合并。{check_rule}"
        )
    } else {
        format!(
        "你是本次 fusion 多模型竞赛的『裁判』。一个需求被 {n} 个互相隔离的选手各自独立实现，\
         现在轮到你评出唯一的赢家并把结果落地。\n\n\
         ## 本次需求\n{need}\n\n\
         ## 主线（base）分支\n`{base_branch}`\n\n\
         ## 参赛选手\n{roster}\n\
         ## 你的任务（务必逐步执行）\n\
         1. 你当前的工作目录就是裁判专用的 git worktree，所有选手的分支都在同一个 git 仓库里可见。\n\
         2. 逐个检查每位选手的改动：对每个选手分支跑 `git diff {base_branch}...<选手分支名>`，\
         逐行读懂它实现了什么、质量如何（正确性、完整性、可读性、是否真正满足需求、有无明显 bug）。\n\
         3. **先产出结构化对比分析（不是投票、不是简单挑一个）**：把各家方案对比成四类——\
         `consensus`（多数方案的共识/相同正确做法）、`contradictions`（各家关键分歧与取舍）、\
         `unique_insights`（只有某一家想到的亮点）、`blind_spots`（所有人都漏掉的点）。\
         用 swarm_write_blackboard 把它写成 JSON 存到 key `{analysis_key}`，形如 \
         `{{\"consensus\":[…],\"contradictions\":[…],\"unique_insights\":[…],\"blind_spots\":[…]}}`。\n\
         4. 基于上面的分析与客观质量，评出**唯一一个**赢家。不要含糊、不要并列。\n\
         5. 评出赢家后，**通过 curl 调用 decide 端点把结果落地**，使用赢家的 thread_id：\n\
         ```\n\
         curl -X POST {decide_url} -H 'content-type: application/json' -d '{{\"winner_thread_id\":\"<选中的thread_id>\"}}'\n\
         ```\n\
         把 `<选中的thread_id>` 换成你选中的那位选手上面列出的 thread_id（形如 `th-xxxx`）。\n\
         6. curl 返回成功（HTTP 200 + 含 winner_thread_id 的 JSON）后，用 swarm_send_message 给 `user` \
         发一句话，说明你选了谁、为什么选它、其它选手输在哪。然后停止。\n\n\
         注意：只读 diff、先写分析再调用一次 decide，不要去改任何选手的代码，也不要自己动手合并——合并由 decide 端点负责。{check_rule}"
        )
    };

    let layout = WorkspaceLayout::Shared {
        dir: std::path::PathBuf::from(judge_cwd),
    };
    let spawn_ms = now_ms();
    let out = spawn_with_bookkeeping(
        state,
        &cli,
        Some("fusion-judge".to_string()),
        None,
        None,
        layout,
        workspace_id.to_string(),
        None,
        Some(judge_thread_id.to_string()),
    )
    .await?;
    spawn_bootstrap_inject(
        state.registry.clone(),
        out.lifecycle_rx.resubscribe(),
        out.agent_id.clone(),
        system_prompt,
        BootstrapCtx {
            source: "worker",
            spell: String::new(),
            role_keys: Vec::new(),
        },
        Vec::new(),
        state.swarm.clone(),
        state.plugins.clone(),
        spawn_ms,
        state.server_url.clone(),
    );
    Ok(out.agent_id)
}

/// Env-tunable ceiling for how long the judge watchdog waits before forcing a
/// decision. Synth judges hand-write code, so default generously (15 min).
fn judge_watchdog_timeout() -> std::time::Duration {
    std::env::var("SWARMX_FUSION_JUDGE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or_else(|| std::time::Duration::from_secs(15 * 60))
}

/// Watch the auto-judge agent and GUARANTEE the batch leaves 'judging'. The
/// only path to decide is the judge LLM's own `curl`; this closes the gap when
/// it doesn't fire. Polls the judge agent's store row; once it's terminal
/// (killed/exited) or idle past the grace, or the max elapses, it runs a
/// deterministic fallback decide. Idempotent with the judge's own decide via the
/// storage CAS. See [`judge_fallback`].
#[allow(clippy::too_many_arguments)]
fn spawn_judge_watchdog(
    state: AppState,
    workspace_id: String,
    batch_id: String,
    judge_thread_id: String,
    judge_agent_id: Option<String>,
    synthesize: bool,
    check_cmd: Option<String>,
    base_branch: Option<String>,
    base_cwd: String,
    contestants: Vec<swarmx_protocol::rest::FusionContestantDiff>,
) {
    tokio::spawn(async move {
        const POLL: std::time::Duration = std::time::Duration::from_secs(5);
        const IDLE_GRACE_MS: i64 = 90_000;
        let max = judge_watchdog_timeout();
        let start = std::time::Instant::now();
        loop {
            tokio::time::sleep(POLL).await;

            // Already decided (judge's own curl, or a human)? Nothing to do.
            if current_batch_status(&state, &workspace_id, &batch_id).await.as_deref() != Some("judging") {
                return;
            }
            let timed_out = start.elapsed() >= max;
            if !timed_out
                && !judge_settled(
                    &state,
                    &judge_thread_id,
                    judge_agent_id.as_deref(),
                    IDLE_GRACE_MS,
                    synthesize,
                    base_branch.as_deref(),
                    &base_cwd,
                )
                .await
            {
                continue;
            }
            // Grace: a judge that JUST finished may be curling decide right now.
            // Let it land (the CAS makes a lost race harmless; this just avoids a
            // redundant fallback merge).
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if current_batch_status(&state, &workspace_id, &batch_id).await.as_deref() != Some("judging") {
                return;
            }
            tracing::warn!(batch = %batch_id, timed_out, "fusion judge watchdog: judge did not decide; running deterministic fallback");
            judge_fallback(
                &state, &workspace_id, &batch_id, &judge_thread_id, synthesize,
                check_cmd.as_deref(), base_branch.as_deref(), &base_cwd, &contestants,
            )
            .await;
            return;
        }
    });
}

/// Current status of a fusion batch, or None if gone.
async fn current_batch_status(state: &AppState, workspace_id: &str, batch_id: &str) -> Option<String> {
    state
        .store
        .list_fusion_batches(workspace_id.to_string())
        .await
        .ok()?
        .into_iter()
        .find(|b| b.id == batch_id)
        .map(|b| b.status)
}

/// Whether the judge watchdog may fire its fallback yet. The signal differs by
/// mode because the CONSEQUENCE differs — `idle-time` is a GUESS that can't tell
/// "finished, waiting" from "still thinking on a long step", so it's only used
/// where an early fire is harmless:
///   - terminal (killed/exited) or a missing row → always yes (the judge is dead;
///     whatever it produced is final).
///   - synth mode (its output gets MERGED): yes only if the judge has COMMITTED
///     its synthesis. NEVER on idle-time — that could commit+merge a half-written
///     file and lose the judge's real output to the CAS.
///   - pick mode: idle past the grace is enough — the fallback re-runs the
///     DETERMINISTIC objective gate (not the judge's opinion) and the CAS blocks
///     any double, so an early fire only wastes a gate re-run.
async fn judge_settled(
    state: &AppState,
    judge_thread_id: &str,
    judge_agent_id: Option<&str>,
    idle_grace_ms: i64,
    synthesize: bool,
    base_branch: Option<&str>,
    base_cwd: &str,
) -> bool {
    let agents = state.store.list_agents().await.unwrap_or_default();
    let judge = match judge_agent_id {
        Some(aid) => agents.iter().find(|a| a.id == aid),
        None => agents
            .iter()
            .find(|a| a.thread_id.as_deref() == Some(judge_thread_id) && a.role == "fusion-judge"),
    };
    let Some(a) = judge else {
        return true; // no row → spawn failed / reaped → truly gone
    };
    if a.killed_at.is_some() || a.shim_exit_at.is_some() {
        return true; // terminal — whatever it wrote is final
    }
    if synthesize {
        // Merge-producing: only a COMMITTED synthesis is safe to act on.
        judge_committed(state, judge_thread_id, base_branch, base_cwd).await
    } else {
        // Pick: idle is safe (the fallback uses the gate, not the judge's output).
        matches!(a.last_activity_at, Some(last) if now_ms() - last > idle_grace_ms)
    }
}

/// Has the judge committed work to its branch vs base? The "done writing" signal
/// for a synth judge — engine-independent and unambiguous (unlike idle-time).
async fn judge_committed(
    state: &AppState,
    judge_thread_id: &str,
    base_branch: Option<&str>,
    base_cwd: &str,
) -> bool {
    let (Some(base), Ok(Some(th))) = (
        base_branch,
        state.store.get_thread(judge_thread_id.to_string()).await,
    ) else {
        return false;
    };
    if th.isolation != "worktree" {
        return false;
    }
    let Some(branch) = th.branch.clone().filter(|b| !b.is_empty()) else {
        return false;
    };
    let repo = base_cwd.to_string();
    let base = base.to_string();
    tokio::task::spawn_blocking(move || {
        crate::worktree::diff_summary(std::path::Path::new(&repo), &base, &branch)
    })
    .await
    .map(|f| !f.is_empty())
    .unwrap_or(false)
}

/// Deterministic decide when the judge agent didn't. Never leaves the batch in
/// 'judging': synth → merge the judge's captured work (empty → needs_decision);
/// pick+check → first contestant to pass the re-run gate (none → needs_decision);
/// no deterministic signal → needs_decision (manual pick in the UI).
#[allow(clippy::too_many_arguments)]
async fn judge_fallback(
    state: &AppState,
    workspace_id: &str,
    batch_id: &str,
    judge_thread_id: &str,
    synthesize: bool,
    check_cmd: Option<&str>,
    base_branch: Option<&str>,
    base_cwd: &str,
    contestants: &[swarmx_protocol::rest::FusionContestantDiff],
) {
    let Ok(ws) = require_workspace(state, workspace_id).await else {
        return;
    };
    let Some(batch) = state
        .store
        .list_fusion_batches(workspace_id.to_string())
        .await
        .ok()
        .and_then(|bs| bs.into_iter().find(|b| b.id == batch_id))
    else {
        return;
    };
    if batch.status != "judging" {
        return;
    }

    let winner: Option<String> = if synthesize {
        // Capture the judge's uncommitted synthesis, then require it produced
        // something — merging an empty branch would silently discard all
        // contestant work.
        match state.store.get_thread(judge_thread_id.to_string()).await {
            Ok(Some(jt)) => {
                let jcwd = jt.cwd.clone();
                let jbranch = jt.branch.clone().unwrap_or_default();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::worktree::commit_worktree_work(
                        std::path::Path::new(&jcwd),
                        "swarmx: capture judge synthesis (watchdog)",
                    )
                })
                .await;
                let has_work = match base_branch {
                    Some(base) if !jbranch.is_empty() => {
                        let repo = base_cwd.to_string();
                        let base = base.to_string();
                        tokio::task::spawn_blocking(move || {
                            crate::worktree::diff_summary(std::path::Path::new(&repo), &base, &jbranch)
                        })
                        .await
                        .map(|f| !f.is_empty())
                        .unwrap_or(false)
                    }
                    _ => false,
                };
                has_work.then(|| judge_thread_id.to_string())
            }
            _ => None,
        }
    } else if let Some(cmd) = check_cmd {
        // Re-run the objective gate now (contestant worktrees are stable);
        // first passing contestant in batch order wins.
        let mut w = None;
        for c in contestants {
            let Some(branch) = c.branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            let dir = crate::worktree::worktree_dest(std::path::Path::new(base_cwd), branch)
                .to_string_lossy()
                .into_owned();
            let (passed, _) = run_contestant_check(&dir, cmd).await;
            if passed {
                w = Some(c.thread_id.clone());
                break;
            }
        }
        w
    } else {
        None
    };

    match winner {
        Some(wid) => match decide_fusion_inner(state, &ws, &batch, &wid, true).await {
            Ok(_) => tracing::info!(batch = %batch_id, winner = %wid, "fusion judge watchdog: decided deterministically"),
            Err((_, e)) => tracing::warn!(batch = %batch_id, err = ?e, "fusion judge watchdog: fallback decide failed"),
        },
        None => {
            // No deterministic signal — never stay stuck; surface manual pick.
            let _ = state
                .store
                .transition_fusion_status(
                    batch_id.to_string(),
                    "judging".to_string(),
                    "needs_decision".to_string(),
                )
                .await;
            tracing::info!(batch = %batch_id, "fusion judge watchdog: no deterministic winner → needs_decision (manual)");
        }
    }
    publish_thread_changed(state, workspace_id, judge_thread_id, "updated");
}

/// `POST /api/workspaces/:id/fusion/:bid/decide` — the verdict / terminal stage.
/// The caller picks ONE winning contestant; we validate it's actually one of the
/// batch's contestants, record it + flip the batch to 'done', then (unless the
/// request says otherwise) merge the winner's branch back into the base line —
/// reusing the exact same merge machinery as the per-direction merge endpoint
/// (uncommitted work is captured first, conflicts spawn an AI resolver agent).
pub async fn decide_fusion_handler(
    State(state): State<AppState>,
    Path((workspace_id, batch_id)): Path<(String, String)>,
    Json(req): Json<swarmx_protocol::rest::FusionDecideRequest>,
) -> Result<Json<swarmx_protocol::rest::FusionDecideResponse>, (StatusCode, Json<serde_json::Value>)> {
    let ws = require_workspace(&state, &workspace_id).await?;
    let batches = state
        .store
        .list_fusion_batches(workspace_id.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    let batch = batches
        .into_iter()
        .find(|b| b.id == batch_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown fusion batch: {batch_id}")})),
            )
        })?;

    match decide_fusion_inner(&state, &ws, &batch, &req.winner_thread_id, req.merge).await? {
        DecideOutcome::Decided(resp) => Ok(Json(resp)),
        DecideOutcome::AlreadyDecided => Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "fusion batch already decided"})),
        )),
    }
}

/// Terminal outcome of [`decide_fusion_inner`]: either this caller claimed the
/// verdict (and we merged / recorded it), or someone else won the CAS race first.
enum DecideOutcome {
    Decided(swarmx_protocol::rest::FusionDecideResponse),
    AlreadyDecided,
}

/// The decide+merge core, callable IN-PROCESS by both the HTTP handler and the
/// judge watchdog (no self-`curl`). Validates the winner, CAS-claims the batch
/// (returns `AlreadyDecided` if the judge's own decide — or a human — already
/// won the race), then unless `merge` is false merges the winner's branch into
/// base with the shared worktree machinery (captures uncommitted work first,
/// spawns an AI resolver on conflict). Idempotency lives entirely in the storage
/// CAS, so concurrent callers can never double-merge.
async fn decide_fusion_inner(
    state: &AppState,
    ws: &swarmx_storage::WorkspaceRecord,
    batch: &swarmx_storage::FusionBatchRecord,
    winner_thread_id: &str,
    merge: bool,
) -> Result<DecideOutcome, (StatusCode, Json<serde_json::Value>)> {
    let workspace_id = ws.id.clone();

    // The winner MUST be one of this batch's contestants — OR the batch's judge
    // thread when the judge synthesized a combined-best implementation into its
    // own worktree (P3.3 synthesis-merge). Never an unrelated thread. SQLite
    // can't constrain a value against a JSON array, so enforce it here.
    let is_synthesis = batch.judge_thread_id.as_deref() == Some(winner_thread_id);
    if !batch.contestant_thread_ids.iter().any(|t| t == winner_thread_id) && !is_synthesis {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "winner_thread_id is not a contestant (or the judge) of this batch"})),
        ));
    }

    // CAS-claim the verdict (winner + status='done'). 0 rows = someone already
    // decided (the judge's curl, the watchdog, a double-click) → no-op.
    let updated = state
        .store
        .set_fusion_winner(batch.id.clone(), winner_thread_id.to_string())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    if updated == 0 {
        return Ok(DecideOutcome::AlreadyDecided);
    }

    // Nudge the Fusion view to refetch: the batch just flipped to 'done', but the
    // view only refetches on swarm activity — and the decider (the judge's own
    // curl OR the watchdog) may emit nothing else, leaving the card stuck on
    // 'judging' until a manual refresh. Emit a thread_changed the view listens to.
    publish_thread_changed(state, &workspace_id, winner_thread_id, "decided");

    // Re-read so the returned batch reflects winner + 'done'.
    let decided = state
        .store
        .list_fusion_batches(workspace_id.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .into_iter()
        .find(|b| b.id == batch.id)
        .map(fusion_record_to_wire)
        .ok_or_else(|| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "batch vanished after decide"})))
        })?;

    // If the caller didn't want a merge, we're done: verdict recorded, no base
    // mutation.
    if !merge {
        return Ok(DecideOutcome::Decided(swarmx_protocol::rest::FusionDecideResponse {
            batch: decided,
            winner_thread_id: winner_thread_id.to_string(),
            merge_status: None,
            base: None,
            files: Vec::new(),
            resolver_agent_id: None,
        }));
    }

    // Merge the winner's branch into base, reusing the per-direction merge logic.
    let winner = require_thread(state, &workspace_id, winner_thread_id).await?;
    let branch = match winner.branch.clone().filter(|b| !b.is_empty()) {
        Some(b) if winner.isolation == "worktree" => b,
        _ => {
            // Winner never got an isolated branch (degraded) — verdict still
            // stands, but there's nothing to merge. Be honest rather than lying
            // "merged".
            return Ok(DecideOutcome::Decided(swarmx_protocol::rest::FusionDecideResponse {
                batch: decided,
                winner_thread_id: winner_thread_id.to_string(),
                merge_status: Some("nothing_to_merge".to_string()),
                base: None,
                files: Vec::new(),
                resolver_agent_id: None,
            }));
        }
    };
    let cwd = std::path::PathBuf::from(&ws.cwd);
    let dir_cwd = std::path::PathBuf::from(&winner.cwd);
    let branch_for_git = branch.clone();
    let result = tokio::task::spawn_blocking(move || {
        if dir_cwd != cwd {
            if let Err(e) = crate::worktree::commit_worktree_work(
                &dir_cwd,
                "swarmx: capture winning direction work before fusion merge",
            ) {
                return Err(format!("提交方向改动失败：{e}"));
            }
        }
        if crate::worktree::working_dirty(&cwd) {
            return Err("主线有未提交改动，请先提交或暂存后再合并".to_string());
        }
        let base = crate::worktree::current_branch(&cwd)
            .ok_or_else(|| "主线处于游离 HEAD，无法合并".to_string())?;
        let outcome = crate::worktree::merge_into_base(&cwd, &base, &branch_for_git);
        Ok((base, outcome))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    let (base, outcome) = match result {
        Ok(v) => v,
        Err(msg) => return Err((StatusCode::CONFLICT, Json(json!({"error": msg})))),
    };

    match outcome {
        crate::worktree::MergeOutcome::Clean { files } => {
            let _ = files; // count of files merged; the wire shape carries names, not a count
            Ok(DecideOutcome::Decided(swarmx_protocol::rest::FusionDecideResponse {
                batch: decided,
                winner_thread_id: winner_thread_id.to_string(),
                merge_status: Some("merged".to_string()),
                base: Some(base),
                files: Vec::new(),
                resolver_agent_id: None,
            }))
        }
        crate::worktree::MergeOutcome::Conflict { files } => {
            let agent_id = spawn_merge_resolver(state, ws, &base, &branch, &files)
                .await
                .map_err(|(s, m)| (s, Json(json!({"error": m}))))?;
            Ok(DecideOutcome::Decided(swarmx_protocol::rest::FusionDecideResponse {
                batch: decided,
                winner_thread_id: winner_thread_id.to_string(),
                merge_status: Some("resolving".to_string()),
                base: Some(base),
                files,
                resolver_agent_id: Some(agent_id),
            }))
        }
        crate::worktree::MergeOutcome::Error { msg } => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("合并失败：{msg}")})),
        )),
    }
}

/// Map a storage FusionBatchRecord onto the wire DTO.
fn fusion_record_to_wire(r: swarmx_storage::FusionBatchRecord) -> swarmx_protocol::rest::FusionBatch {
    swarmx_protocol::rest::FusionBatch {
        id: r.id,
        workspace_id: r.workspace_id,
        slug: r.slug,
        need: r.need,
        contestant_thread_ids: r.contestant_thread_ids,
        judge_thread_id: r.judge_thread_id,
        status: r.status,
        winner_thread_id: r.winner_thread_id,
        check_cmd: r.check_cmd,
        created_at: r.created_at,
    }
}

/// Unique batch slug among a workspace's alive fusion batches (mirrors
/// `unique_thread_slug`). Best-effort: on a list error returns `base`.
async fn unique_fusion_slug(state: &AppState, workspace_id: &str, base: &str) -> String {
    let existing: std::collections::HashSet<String> = state
        .store
        .list_fusion_batches(workspace_id.to_string())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.slug)
        .collect();
    if !existing.contains(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}-{n}");
        if !existing.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

/// Broadcast that a workspace's direction (thread) list changed so subscribers
/// (the sidebar) refetch `/api/workspaces`. The REST snapshot stays the source
/// of truth; this is just the "now" signal for a change the snapshot can't push
/// itself — notably `swarm_name_thread` → background worktree isolation, which
/// renames + flips the branch icon without any UI-initiated request.
fn publish_thread_changed(state: &AppState, workspace_id: &str, thread_id: &str, op: &str) {
    state.swarm.publish_event(SwarmEvent::ThreadChanged {
        workspace_id: workspace_id.to_string(),
        thread_id: thread_id.to_string(),
        op: op.to_string(),
    });
}

// ── merge a direction back into the main line ────────────────────────────────

#[derive(serde::Serialize)]
pub struct ThreadDiffResponse {
    /// Base branch = the branch currently checked out at the workspace cwd.
    pub base: Option<String>,
    /// The direction's own branch (None for a shared/non-isolated direction).
    pub branch: Option<String>,
    /// Repo-relative files this direction changed vs the merge-base.
    pub files: Vec<String>,
    /// Whether the base work tree has uncommitted changes — a merge would be
    /// refused, so the UI warns and disables the merge button.
    pub base_dirty: bool,
}

/// `GET /api/workspaces/:id/threads/:tid/diff` — preview "what this direction
/// changed" before merging. Read-only; never touches the work tree.
pub async fn thread_diff_handler(
    State(state): State<AppState>,
    Path((workspace_id, thread_id)): Path<(String, String)>,
) -> Result<Json<ThreadDiffResponse>, (StatusCode, Json<serde_json::Value>)> {
    let ws = require_workspace(&state, &workspace_id).await?;
    let th = require_thread(&state, &workspace_id, &thread_id).await?;
    let branch = th.branch.clone().filter(|b| !b.is_empty());
    let cwd = std::path::PathBuf::from(&ws.cwd);
    let branch_for_diff = branch.clone();
    let (base, files, base_dirty) = tokio::task::spawn_blocking(move || {
        let base = crate::worktree::current_branch(&cwd);
        let files = match (&base, &branch_for_diff) {
            (Some(b), Some(f)) => crate::worktree::diff_summary(&cwd, b, f),
            _ => Vec::new(),
        };
        let dirty = crate::worktree::working_dirty(&cwd);
        (base, files, dirty)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
    })?;
    Ok(Json(ThreadDiffResponse {
        base,
        branch,
        files,
        base_dirty,
    }))
}

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MergeResponse {
    /// Merged cleanly into `base`; `files` is the direction's changed-file count.
    Merged { base: String, files: usize },
    /// Conflicts; an AI resolver agent was spawned to finish the merge.
    Resolving {
        agent_id: String,
        files: Vec<String>,
    },
}

/// `POST /api/workspaces/:id/threads/:tid/merge` — merge a direction's branch
/// back into the main line. Clean → done. Conflict → spawn an AI agent in the
/// main worktree to resolve + commit (the user just sees "AI is reconciling").
pub async fn merge_thread_handler(
    State(state): State<AppState>,
    Path((workspace_id, thread_id)): Path<(String, String)>,
) -> Result<Json<MergeResponse>, (StatusCode, Json<serde_json::Value>)> {
    let ws = require_workspace(&state, &workspace_id).await?;
    let th = require_thread(&state, &workspace_id, &thread_id).await?;
    let branch = match th.branch.clone().filter(|b| !b.is_empty()) {
        Some(b) if th.isolation == "worktree" => b,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "该方向没有独立分支，无需合并"})),
            ));
        }
    };
    let cwd = std::path::PathBuf::from(&ws.cwd);
    let dir_cwd = std::path::PathBuf::from(&th.cwd);

    // All git work on a blocking thread (shells out). Returns the base branch +
    // the merge outcome, or a user-facing refusal string.
    let branch_for_git = branch.clone();
    let result = tokio::task::spawn_blocking(move || {
        // W1-2: workers edit files in the isolated direction worktree but are
        // never told to `git commit`, and merge_into_base brings only COMMITTED
        // content — so an un-committed worktree would merge as an empty/stale
        // branch (data loss + a lying "Merged"). Capture that work as a commit
        // on the direction's branch FIRST. Guarded by `dir_cwd != cwd` so it
        // only ever touches the isolated direction worktree, never the base —
        // the base's own uncommitted-changes red line below stays intact.
        if dir_cwd != cwd {
            if let Err(e) = crate::worktree::commit_worktree_work(
                &dir_cwd,
                "swarmx: capture direction work before merge",
            ) {
                return Err(format!("提交方向改动失败：{e}"));
            }
        }
        if crate::worktree::working_dirty(&cwd) {
            return Err("主线有未提交改动，请先提交或暂存后再合并".to_string());
        }
        let base = crate::worktree::current_branch(&cwd)
            .ok_or_else(|| "主线处于游离 HEAD，无法合并".to_string())?;
        let outcome = crate::worktree::merge_into_base(&cwd, &base, &branch_for_git);
        Ok((base, outcome))
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
    })?;
    let (base, outcome) = match result {
        Ok(v) => v,
        // A clean refusal (dirty base / detached HEAD) — 409 so the UI shows the
        // reason rather than a generic error.
        Err(msg) => return Err((StatusCode::CONFLICT, Json(json!({"error": msg})))),
    };

    match outcome {
        crate::worktree::MergeOutcome::Clean { files } => {
            Ok(Json(MergeResponse::Merged { base, files }))
        }
        crate::worktree::MergeOutcome::Conflict { files } => {
            let agent_id = spawn_merge_resolver(&state, &ws, &base, &branch, &files)
                .await
                .map_err(|(s, m)| (s, Json(json!({"error": m}))))?;
            Ok(Json(MergeResponse::Resolving { agent_id, files }))
        }
        crate::worktree::MergeOutcome::Error { msg } => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("合并失败：{msg}")})),
        )),
    }
}

/// `POST /api/workspaces/:id/fusion-consult` — the answer/research fusion
/// (panel → judge → synthesis), backed by the zulu model panel. Distinct from
/// the code-competition fusion above. Requires a configured Comate license.
pub async fn fusion_consult_handler(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<swarmx_protocol::rest::FusionConsultRequest>,
) -> Result<Json<swarmx_protocol::rest::FusionConsultResponse>, (StatusCode, Json<serde_json::Value>)>
{
    let ws = require_workspace(&state, &workspace_id).await?;
    let license = crate::comate::load_license();
    if license.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "未配置 Comate License（设置 → 插件）"})),
        ));
    }
    crate::fusion::consult(&req, &license, &ws.cwd)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })
}

async fn require_workspace(
    state: &AppState,
    workspace_id: &str,
) -> Result<swarmx_storage::WorkspaceRecord, (StatusCode, Json<serde_json::Value>)> {
    state
        .store
        .get_workspace_by_id(workspace_id.to_string())
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
                Json(json!({"error": format!("unknown workspace_id: {workspace_id}")})),
            )
        })
}

async fn require_thread(
    state: &AppState,
    workspace_id: &str,
    thread_id: &str,
) -> Result<swarmx_storage::ThreadRecord, (StatusCode, Json<serde_json::Value>)> {
    state
        .store
        .get_thread(thread_id.to_string())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        // Reject a thread that belongs to a *different* workspace than the one in
        // the path — the sibling handlers (update/delete/set-model) already
        // filter this way; merge/diff/decide must not operate cross-workspace.
        .filter(|t| t.workspace_id == workspace_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown thread_id: {thread_id}")})),
            )
        })
}

/// Pick the CLI for the merge-resolver: prefer this workspace's live
/// orchestrator (so it matches what the user configured), else any live agent,
/// else "claude" (the safe default — conflict resolution wants a strong model).
async fn resolver_cli(state: &AppState, workspace_id: &str) -> String {
    if let Ok(agents) = state.store.list_agents().await {
        let alive = |a: &&swarmx_storage::AgentRecord| {
            a.workspace_id.as_deref() == Some(workspace_id)
                && a.killed_at.is_none()
                && a.shim_exit_at.is_none()
        };
        if let Some(a) = agents.iter().find(|a| alive(a) && a.role == "orchestrator") {
            return a.cli.clone();
        }
        if let Some(a) = agents.iter().find(alive) {
            return a.cli.clone();
        }
    }
    "claude".to_string()
}

/// Spawn a one-shot agent in the MAIN worktree (ws.cwd, currently mid-merge with
/// conflict markers + MERGE_HEAD) to resolve the conflicts and commit. Lands it
/// in the main direction (thread_id = main) so it operates on the primary
/// worktree, not the direction being merged.
async fn spawn_merge_resolver(
    state: &AppState,
    ws: &swarmx_storage::WorkspaceRecord,
    base: &str,
    branch: &str,
    files: &[String],
) -> Result<String, (StatusCode, String)> {
    let main_thread_id = state
        .store
        .list_threads(ws.id.clone())
        .await
        .ok()
        .and_then(|ts| ts.into_iter().find(|t| t.slug == "main").map(|t| t.id));
    let cli = resolver_cli(state, &ws.id).await;
    let files_list = files.join("、");
    let system_prompt = format!(
        "你正在解决一次 git merge 冲突：把分支 `{branch}` 合并进 `{base}` 时与主线已有改动撞车了。\n\
         冲突文件：{files_list}。\n\
         逐个打开这些文件，理解两边各自想做什么，消除所有 `<<<<<<<` / `=======` / `>>>>>>>` 冲突标记，\
         保留两边都合理的意图（不是无脑选一边）。改完后对这些文件 `git add`，再 `git commit`（保留默认的 \
         merge commit 信息）完成合并。\n\
         完成后用 swarm_send_message 给 `user` 发一句话，说明你把什么和什么调和了；若某处你不确定，也在这句话里点出来。然后停止。\n\
         只动这些冲突文件，不要改与本次冲突无关的任何东西。"
    );
    let layout = WorkspaceLayout::Shared {
        dir: std::path::PathBuf::from(&ws.cwd),
    };
    let spawn_ms = now_ms();
    let out = spawn_with_bookkeeping(
        state,
        &cli,
        Some("merge-resolver".to_string()),
        None,
        None,
        layout,
        ws.id.clone(),
        None,
        main_thread_id,
    )
    .await?;
    // Inject immediately (no dependency gate — there's nothing to wait on).
    spawn_bootstrap_inject(
        state.registry.clone(),
        out.lifecycle_rx.resubscribe(),
        out.agent_id.clone(),
        system_prompt,
        BootstrapCtx {
            source: "worker",
            spell: String::new(),
            role_keys: Vec::new(),
        },
        Vec::new(),
        state.swarm.clone(),
        state.plugins.clone(),
        spawn_ms,
        state.server_url.clone(),
    );
    Ok(out.agent_id)
}

/// `PATCH /api/workspaces/:id/threads/:tid` — (re)name a direction. Naming a
/// previously-unnamed, non-`main` direction is ALSO the trigger for automatic
/// git isolation: we persist the name + branch, flip state to `preparing`,
/// return immediately, and a background task does git takeover + `worktree add`
/// + repoints the thread cwd, finally → `ready`. If git isolation fails (or the
/// project can't be a repo) the direction degrades gracefully to `shared` and
/// stays usable. The `main` direction and already-isolated threads are a pure
/// rename. The slug NEVER changes (keeps blackboard keys stable).
///
/// NOTE (P3 boundary): repointing the cwd does not migrate an ALREADY-running
/// agent's process cwd. The intended flow names the direction from the first
/// message (before file work) and the frontend gates orchestrator/worker spawns
/// on `state == "ready"`, restarting into the new cwd. Sequencing lives in P4/P5.
pub async fn update_thread_handler(
    State(state): State<AppState>,
    Path((workspace_id, thread_id)): Path<(String, String)>,
    Json(req): Json<UpdateThreadRequest>,
) -> Result<Json<ThreadInfo>, (StatusCode, Json<serde_json::Value>)> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name must be non-empty"})),
        ));
    }
    let thread = state
        .store
        .get_thread(thread_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .filter(|t| t.workspace_id == workspace_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown thread: {thread_id}")})),
            )
        })?;

    // Kick off git isolation only for a not-yet-isolated, non-main direction
    // that isn't already mid-isolation. The `state != "preparing"` guard makes
    // a second concurrent PATCH (double-click / retried swarm_name_thread) a
    // pure rename instead of racing a second `git worktree add` on the same
    // dest. A degraded thread is back at `ready`, so it can still retry.
    let should_isolate =
        thread.slug != "main" && thread.isolation != "worktree" && thread.state != "preparing";

    if should_isolate {
        // Branch (and thus the worktree dir `<project>-<branch>`) is derived
        // from the STABLE per-workspace-unique slug, NOT the raw name: two
        // directions sharing a display name still get distinct slugs, so they
        // can't collide on the same branch/worktree dir. `name` only updates
        // the display label.
        let branch = thread.slug.clone();
        // Phase 1 (sync, fast): name + `preparing`. branch/isolation/cwd are
        // persisted in phase 2 only once the worktree actually exists, so a
        // failed isolation never leaves a stale branch on a shared thread.
        state
            .store
            .update_thread(
                thread_id.clone(),
                Some(name.to_string()),
                None, // slug stays stable
                None, // isolation flips in phase 2 on success
                None, // branch persisted in phase 2 on success
                None, // cwd repoints in phase 2 on success
                Some("preparing".to_string()),
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            })?;
        // Phase 2 (background): git takeover + worktree add → ready.
        spawn_thread_worktree(
            state.clone(),
            thread_id.clone(),
            workspace_id.clone(),
            thread.cwd.clone(),
            branch,
        );
    } else {
        state
            .store
            .update_thread(
                thread_id.clone(),
                Some(name.to_string()),
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            })?;
    }

    // Phase-1 rename / `preparing` flip is persisted now — tell the sidebar.
    // (Isolation success/degrade fires a second `ThreadChanged` from phase 2.)
    publish_thread_changed(&state, &workspace_id, &thread_id, "updated");

    let updated = state
        .store
        .get_thread(thread_id.clone())
        .await
        .ok()
        .flatten()
        .map(thread_record_to_info)
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "thread vanished after update"})),
            )
        })?;
    Ok(Json(updated))
}

#[derive(serde::Deserialize)]
pub struct SetThreadModelRequest {
    /// Abstract tier (opus|sonnet|haiku) or a concrete model id. Empty / null =
    /// clear (use the global default). The body carries the COMPLETE desired
    /// state — both fields are always written — so there's no absent-vs-clear
    /// ambiguity.
    #[serde(default)]
    pub tier: Option<String>,
    /// Abstract reasoning effort (low|medium|high|max). Empty / null = clear
    /// (the model's own default).
    #[serde(default)]
    pub reasoning: Option<String>,
}

/// PUT /api/workspaces/:id/threads/:tid/model — set (or clear) this direction's
/// model AND reasoning effort (the body is the full desired state). Persists
/// only; takes effect on the NEXT spawn in the direction (the client restarts
/// the orchestrator to apply it now). Returns the updated thread.
pub async fn set_thread_model_handler(
    State(state): State<AppState>,
    Path((workspace_id, thread_id)): Path<(String, String)>,
    Json(req): Json<SetThreadModelRequest>,
) -> Result<Json<ThreadInfo>, (StatusCode, Json<serde_json::Value>)> {
    // Confirm the thread exists and belongs to this workspace.
    let thread = state
        .store
        .get_thread(thread_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .filter(|t| t.workspace_id == workspace_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown thread: {thread_id}")})),
            )
        })?;
    // Capture the old model state before discarding the row — the model_changed
    // card below reports the from→to.
    let old_tier = thread.model_tier.clone();
    let old_reasoning = thread.reasoning_effort.clone();
    // Normalize empty → None (clear). A concrete tier/model/effort is stored
    // verbatim and resolved per-CLI at spawn time.
    let tier = req
        .tier
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let reasoning = req
        .reasoning
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Validate effort here too (parity with PUT /api/models) — a bogus level
    // would otherwise reach `--effort` at spawn. (tier is a free model id /
    // alias, validated per-CLI at resolve time, so it's not constrained here.)
    if let Some(r) = reasoning.as_deref() {
        if !["low", "medium", "high", "xhigh", "max"].contains(&r) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": format!("invalid reasoning effort '{r}' — valid: low|medium|high|xhigh|max")}),
                ),
            ));
        }
    }
    state
        .store
        .set_thread_model_tier(thread_id.clone(), tier.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    state
        .store
        .set_thread_reasoning_effort(thread_id.clone(), reasoning.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    // P1: surface the model switch in the conversation (入流律). Switching the
    // model restarts the captain mid-turn; a persisted system card makes that
    // visible in the thread instead of the captain silently rebooting. Scoped to
    // this direction's live orchestrator (from="system" → send_message lands it
    // in that agent's thread). Best-effort; only when the model actually changed.
    if old_tier != tier || old_reasoning != reasoning {
        let label = |t: &Option<String>, r: &Option<String>| -> String {
            let base = t.as_deref().unwrap_or("默认").to_string();
            match r.as_deref() {
                Some(eff) => format!("{base}·{eff}"),
                None => base,
            }
        };
        let from = label(&old_tier, &old_reasoning);
        let to = label(&tier, &reasoning);
        if let Ok(agents) = state.store.list_agents().await {
            if let Some(orch) = agents.into_iter().find(|a| {
                a.role == "orchestrator"
                    && a.killed_at.is_none()
                    && a.thread_id.as_deref() == Some(thread_id.as_str())
            }) {
                if let Err(e) = state
                    .swarm
                    .send_message(swarmx_swarm::NewMessage {
                        from_agent: "system".to_string(),
                        to_agent: orch.id,
                        kind: "system".to_string(),
                        body: format!("队长模型已切换 {from} → {to}"),
                        sent_at: now_ms(),
                        in_reply_to: None,
                        meta: Some(serde_json::json!({
                            "subtype": "model_changed",
                            "from": from,
                            "to": to,
                        })),
                    })
                    .await
                {
                    tracing::warn!(?e, "model_changed system card emit failed");
                }
            }
        }
    }
    publish_thread_changed(&state, &workspace_id, &thread_id, "updated");
    let updated = state
        .store
        .get_thread(thread_id.clone())
        .await
        .ok()
        .flatten()
        .map(thread_record_to_info)
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "thread vanished after update"})),
            )
        })?;
    Ok(Json(updated))
}

/// Background git isolation for a freshly-named direction. The git calls block
/// (each internally time-boxed at 30s) so they run on a blocking thread off the
/// async runtime. On success the thread is repointed to the worktree dir and
/// marked `worktree`/`ready`; on ANY failure it degrades to `shared`/`ready`
/// (cwd unchanged) so the direction stays usable, just not isolated.
fn spawn_thread_worktree(
    state: AppState,
    thread_id: String,
    workspace_id: String,
    project_cwd: String,
    branch: String,
) {
    tokio::spawn(async move {
        let cwd_for_git = project_cwd.clone();
        let branch_for_git = branch.clone();
        let git_result = tokio::task::spawn_blocking(move || {
            let p = std::path::Path::new(&cwd_for_git);
            crate::worktree::isolate_into_worktree(p, &branch_for_git)
        })
        .await;
        match git_result {
            Ok(Ok(dest)) => {
                let dest_str = dest.to_string_lossy().into_owned();
                let update = state
                    .store
                    .update_thread(
                        thread_id.clone(),
                        None,
                        None,
                        Some("worktree".to_string()),
                        Some(branch.clone()),
                        Some(dest_str.clone()),
                        Some("ready".to_string()),
                    )
                    .await;
                // Was the direction soft-deleted while we were isolating?
                // `update_thread` guards on `deleted_at IS NULL`, so the write
                // above no-ops on a mid-flight delete — detect it by re-reading
                // and DON'T re-root into / leak a dead direction.
                let deleted = !matches!(
                    state.store.get_thread(thread_id.clone()).await,
                    Ok(Some(t)) if t.deleted_at.is_none()
                );
                if deleted {
                    tracing::info!(thread = %thread_id, "direction deleted during isolation; removing orphaned worktree");
                    let repo = project_cwd.clone();
                    let d = dest_str.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::worktree::worktree_remove(
                            std::path::Path::new(&repo),
                            std::path::Path::new(&d),
                        )
                    })
                    .await;
                } else if let Err(e) = update {
                    // The worktree exists on disk but we couldn't record it.
                    // Degrade to shared/ready so the direction is never left
                    // stuck in `preparing` (the worktree dir is orphaned —
                    // acceptable; it's reusable on a later same-slug retry).
                    tracing::warn!(?e, thread = %thread_id, "worktree built but thread update failed; degrading to shared");
                    degrade_thread_to_shared(&state, &thread_id).await;
                    publish_thread_changed(&state, &workspace_id, &thread_id, "updated");
                } else {
                    tracing::info!(thread = %thread_id, dest = %dest_str, "direction isolated in git worktree");
                    // Re-emit the workspace deps-context into the worktree so the
                    // re-rooted orchestrator (whose new cwd is the worktree, a
                    // copy of the cwd repo only) can still see the peer/dependency
                    // projects at their real paths — otherwise it loses sight of
                    // repos the user may be asking it to work on.
                    write_deps_context_into_dir(&state, &workspace_id, &dest_str).await;
                    // P5-D: re-root the orchestrator into the fresh worktree.
                    // The orchestrator that named the direction is still
                    // running in the OLD (shared) cwd, so its own edits + any
                    // workers it dispatches would split-brain across two dirs.
                    reroot_thread_orchestrator(&state, &workspace_id, &thread_id, &dest_str).await;
                    // worktree/ready + cwd repointed → sidebar flips to the
                    // branch icon + worktree path live (no reload).
                    publish_thread_changed(&state, &workspace_id, &thread_id, "isolated");
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(?e, thread = %thread_id, "git isolation failed; direction stays shared");
                degrade_thread_to_shared(&state, &thread_id).await;
                publish_thread_changed(&state, &workspace_id, &thread_id, "updated");
            }
            Err(e) => {
                tracing::warn!(?e, thread = %thread_id, "worktree task panicked; direction stays shared");
                degrade_thread_to_shared(&state, &thread_id).await;
                publish_thread_changed(&state, &workspace_id, &thread_id, "updated");
            }
        }
    });
}

/// Mark a direction `degraded`/`ready` after a failed isolation attempt. We use
/// a distinct `isolation = "degraded"` (not plain `shared`) so the sidebar can
/// SIGNAL that isolation was attempted and failed — otherwise it looks identical
/// to a not-yet-isolated direction and the user wrongly believes their work is
/// isolated when it's actually sharing the main cwd (two directions' agents then
/// clobber each other's files). `degraded != "worktree"`, so a later rename
/// still retries isolation, and the delete path still skips worktree removal.
async fn degrade_thread_to_shared(state: &AppState, thread_id: &str) {
    let _ = state
        .store
        .update_thread(
            thread_id.to_string(),
            None,
            None,
            Some("degraded".to_string()),
            None,
            None,
            Some("ready".to_string()),
        )
        .await;
}

/// P5-D: after a direction is isolated into a worktree, re-root its orchestrator
/// there. The orchestrator that named the direction is still running in the OLD
/// (shared) cwd; restarting it in the worktree keeps its self-edits + any
/// workers it dispatches from splitting across two directories. We kill the
/// direction's live agents (naming happens BEFORE any worker is dispatched, per
/// orchestrator.md, so this is usually just the orchestrator) and re-run `init`
/// in the new cwd — the fresh orchestrator reads the existing ledger (Phase A
/// short-circuit) and continues. Only ever fires for a git-isolated direction.
async fn reroot_thread_orchestrator(
    state: &AppState,
    workspace_id: &str,
    thread_id: &str,
    new_cwd: &str,
) {
    // Agents we tear down here — their as-yet-unread user messages get
    // re-addressed to the fresh orchestrator below so nothing is dropped.
    let mut killed_ids: Vec<String> = Vec::new();
    match state.store.list_agents().await {
        Ok(rows) => {
            for a in rows {
                if a.thread_id.as_deref() == Some(thread_id)
                    && a.killed_at.is_none()
                    && a.shim_exit_at.is_none()
                {
                    teardown_agent(state, &a.id).await;
                    killed_ids.push(a.id);
                }
            }
            // Naming is supposed to happen BEFORE any worker is dispatched
            // (orchestrator.md), so this should usually kill just the
            // orchestrator. If it killed more, a worker was torn down mid-task
            // (its old-cwd work is abandoned) — surface the invariant breach.
            if killed_ids.len() > 1 {
                tracing::warn!(
                    thread = %thread_id, killed = killed_ids.len(),
                    "re-root tore down >1 agent — a worker was dispatched before naming"
                );
            }
        }
        Err(e) => {
            tracing::warn!(?e, thread = %thread_id, "re-root: list_agents failed; old agents may linger in the shared cwd");
        }
    }
    // The first orchestrator read the user's opening request to NAME this
    // direction, then was torn down (above) BEFORE it wrote a ledger — so the
    // fresh orchestrator would otherwise have neither the ledger nor the
    // (already-read) message and would re-onboard from scratch. Seed its
    // `{task}` with that request so its first turn acts on it instead of asking
    // the user what they want all over again. Best-effort: empty seed on any
    // error just reverts to the old first-wake greeting.
    let seed_task = state
        .store
        .latest_user_message_for_agents(killed_ids.clone())
        .await
        .unwrap_or(None)
        .unwrap_or_default();
    let req = RunSpellRequest {
        name: "init".into(),
        task: seed_task,
        workspace_dir: Some(new_cwd.to_string()),
        workspace_id: Some(workspace_id.to_string()),
        caller_agent_id: None,
        thread_id: Some(thread_id.to_string()),
        // Preserve the captain engine across the rename-respawn.
        captain_cli: super::rest::last_orchestrator_cli(&state, workspace_id).await,
    };
    match run_spell(State(state.clone()), Json(req)).await {
        Ok(Json(resp)) => {
            // Hand the killed orchestrator's unanswered user messages to the
            // fresh one (new agent id) so the first message that triggered the
            // rename isn't stranded on a dead inbox.
            if let Some(orch) = resp.agents.iter().find(|a| a.role == "orchestrator") {
                match state
                    .store
                    .reassign_unread_user_messages(killed_ids.clone(), orch.agent_id.clone())
                    .await
                {
                    Ok(n) if n > 0 => tracing::info!(
                        thread = %thread_id, moved = n, new_orch = %orch.agent_id,
                        "re-root: moved unread user messages to the new orchestrator"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        ?e, thread = %thread_id,
                        "re-root: reassigning unread user messages failed"
                    ),
                }
            }
        }
        Err((status, _)) => {
            tracing::warn!(
                %status, thread = %thread_id,
                "re-root orchestrator after isolation failed (revive on demand)"
            );
        }
    }
}

/// `DELETE /api/workspaces/:id/threads/:tid` — soft-delete a direction (its
/// slug becomes reusable). A git worktree, if any, is removed best-effort in
/// the background. The `main` direction cannot be deleted.
pub async fn delete_thread_handler(
    State(state): State<AppState>,
    Path((workspace_id, thread_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let thread = state
        .store
        .get_thread(thread_id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .filter(|t| t.workspace_id == workspace_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown thread: {thread_id}")})),
            )
        })?;
    if thread.slug == "main" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "the main direction cannot be deleted"})),
        ));
    }
    // Kill the direction's live agents FIRST. Otherwise, once the thread row is
    // gone, those agents are orphaned: P4's strict thread-scoping hides them
    // from chat/DAG/members so the user can't kill them, and `git worktree
    // remove --force` below would yank a still-running agent's cwd out from
    // under it. (Workspace delete deliberately does NOT kill its agents — a
    // direction is the opposite: its agents have nowhere left to live.)
    match state.store.list_agents().await {
        Ok(rows) => {
            for a in rows {
                if a.thread_id.as_deref() == Some(thread_id.as_str())
                    && a.killed_at.is_none()
                    && a.shim_exit_at.is_none()
                {
                    teardown_agent(&state, &a.id).await;
                }
            }
        }
        Err(e) => {
            // Don't soft-delete + force-remove the worktree while we may have
            // failed to kill live agents in it — fail loud instead of orphaning.
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("could not enumerate agents to stop before delete: {e}")}),
                ),
            ));
        }
    }
    state
        .store
        .soft_delete_thread(thread_id.clone(), now_ms())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    publish_thread_changed(&state, &workspace_id, &thread_id, "deleted");

    // Drop the direction's blackboard ledgers (`<ws>/<slug>/…`) so they don't
    // orphan in the panel once the slug is gone. Applies to shared directions
    // too (they still wrote ledgers under the prefix). Best-effort: DB rows
    // first, then the on-disk dir — the notify watcher just sees a removal.
    let bb_prefix = format!("{}/{}", workspace_id, thread.slug);
    if let Err(e) = state
        .store
        .delete_blackboard_prefix(bb_prefix.clone())
        .await
    {
        tracing::warn!(?e, prefix = %bb_prefix, "failed to delete direction blackboard ops");
    }
    let bb_dir = state.blackboard_root.join(&workspace_id).join(&thread.slug);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_dir_all(&bb_dir);
    })
    .await;

    // Best-effort worktree cleanup. repo = the workspace's primary cwd; dest =
    // the thread's worktree dir.
    if thread.isolation == "worktree" {
        if let Ok(Some(ws)) = state.store.get_workspace_by_id(workspace_id).await {
            let repo = ws.cwd.clone();
            let dest = thread.cwd.clone();
            let branch = thread.branch.clone();
            tokio::spawn(async move {
                let _ = tokio::task::spawn_blocking(move || {
                    let repo_path = std::path::Path::new(&repo);
                    // Remove the worktree FIRST — git refuses to delete a branch
                    // still checked out in one. Then drop the now-orphaned branch
                    // so a same-named direction recreated later starts fresh
                    // instead of re-attaching this branch's history.
                    let _ =
                        crate::worktree::worktree_remove(repo_path, std::path::Path::new(&dest));
                    if let Some(b) = branch.as_deref().filter(|b| !b.is_empty()) {
                        let _ = crate::worktree::delete_branch(repo_path, b);
                    }
                })
                .await;
            });
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/workspaces/:id` — soft-delete a workspace. Live agents
/// in the workspace are intentionally NOT killed; the row just stops
/// showing up in `GET /api/workspaces` so the left nav loses it. Anyone
/// still attached via the WS keeps their PTY alive, by design.
pub async fn delete_workspace_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state
        .store
        .soft_delete_workspace(id.clone(), now_ms())
        .await
    {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("workspace {id} not found or already deleted")})),
        ),
        Ok(_) => {
            // Stop this workspace's scheduled jobs from firing into a workspace
            // that's gone (the scheduler would keep trying to revive an
            // orchestrator). Non-destructive — the rows stay, shown as orphaned
            // on /cron. Best-effort: the workspace delete already committed.
            if let Err(e) = state.store.disable_cron_jobs_for_workspace(id.clone()).await {
                tracing::warn!(?e, ws_id = %id, "disable_cron_jobs_for_workspace failed");
            }
            (StatusCode::NO_CONTENT, Json(json!({"ok": true})))
        }
        Err(e) => {
            tracing::warn!(?e, ws_id = %id, "soft_delete_workspace failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    }
}

/// Re-derive and rewrite the workspace's swarmx-managed deps context
/// block from the current set of attached roots. Call this after any
/// add/delete so CLAUDE.md / AGENTS.md stay in sync (and the block is
/// stripped once the last root is removed). Best-effort: store errors are
/// logged and swallowed — the membership change already committed and the
/// context file is advisory, never load-bearing.
async fn refresh_workspace_deps_context(state: &AppState, workspace_id: &str) {
    let ws = match state
        .store
        .get_workspace_by_id(workspace_id.to_string())
        .await
    {
        Ok(Some(ws)) => ws,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(?e, ws_id = %workspace_id, "refresh deps context: get_workspace_by_id failed");
            return;
        }
    };
    // Don't touch the context file of a soft-deleted workspace.
    if ws.deleted_at.is_some() {
        return;
    }
    let roots: Vec<WorkspaceRoot> = match state
        .store
        .list_workspace_roots(workspace_id.to_string())
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| WorkspaceRoot {
                id: r.id,
                path: r.path,
                role: r.role,
                label: r.label,
                parent_id: r.parent_id,
                branch: None, // deps-context writer ignores branch
            })
            .collect(),
        Err(e) => {
            tracing::warn!(?e, ws_id = %workspace_id, "refresh deps context: list_workspace_roots failed");
            return;
        }
    };
    write_workspace_deps_context(ws.cwd.trim(), &ws.name, &roots);
}

/// Write the managed deps-context block (CLAUDE.md / AGENTS.md) into an
/// ARBITRARY directory — used when a direction is isolated into a git worktree.
/// The worktree is a copy of the cwd repo ONLY; the peer/dependency roots live
/// at their original absolute paths and are NOT carried in. Without re-emitting
/// the context here, the re-rooted orchestrator's new cwd (the worktree) has no
/// record that the peer projects exist, so it loses sight of repos the user may
/// actually be asking it to work on. We list the worktree as the primary and the
/// peers at their real paths (still readable), restoring multi-root visibility.
async fn write_deps_context_into_dir(state: &AppState, workspace_id: &str, target_dir: &str) {
    let ws = match state
        .store
        .get_workspace_by_id(workspace_id.to_string())
        .await
    {
        Ok(Some(ws)) => ws,
        _ => return,
    };
    if ws.deleted_at.is_some() {
        return;
    }
    let roots: Vec<WorkspaceRoot> = match state
        .store
        .list_workspace_roots(workspace_id.to_string())
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| WorkspaceRoot {
                id: r.id,
                path: r.path,
                role: r.role,
                label: r.label,
                parent_id: r.parent_id,
                branch: None,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(?e, ws_id = %workspace_id, "worktree deps context: list_workspace_roots failed");
            return;
        }
    };
    // No peer/dependency roots → nothing worth re-emitting in the worktree.
    if roots.is_empty() {
        return;
    }
    write_workspace_deps_context(target_dir, &ws.name, &roots);
}

/// `POST /api/workspaces/:id/roots` — attach a dependency-source root to an
/// existing workspace. Mirrors the per-root validation in
/// `create_workspace_handler` (exists + is a dir → 4xx) and rejects
/// duplicates already attached to this workspace. On success the managed
/// context block in CLAUDE.md / AGENTS.md is refreshed.
pub async fn add_workspace_root_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<WorkspaceRoot>,
) -> Result<Json<WorkspaceRoot>, (StatusCode, Json<serde_json::Value>)> {
    // 404 if the workspace is missing or soft-deleted.
    let ws = state
        .store
        .get_workspace_by_id(id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .filter(|ws| ws.deleted_at.is_none())
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("workspace {id} not found")})),
            )
        })?;

    let path = req.path.trim();
    if path.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "dependency path must be non-empty"})),
        ));
    }
    {
        let p = std::path::Path::new(path);
        if !p.exists() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("dependency directory does not exist: {}", path)})),
            ));
        }
        if !p.is_dir() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("dependency path is not a directory: {}", path)})),
            ));
        }
    }

    // Reject a duplicate already attached to this workspace.
    let existing = state
        .store
        .list_workspace_roots(ws.id.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    if existing.iter().any(|r| r.path == path) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("dependency already attached: {}", path)})),
        ));
    }

    // If a parent was supplied, it must be an existing node in THIS
    // workspace's tree. A parent in another workspace (or a stale id) is a
    // client bug — 400. A genuinely-missing id is a 404.
    let parent_id = match req
        .parent_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(pid) => {
            let parent = state
                .store
                .get_workspace_root(pid.to_string())
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
                        Json(json!({"error": format!("parent root {pid} not found")})),
                    )
                })?;
            if parent.workspace_id != ws.id {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!(
                        "parent root {pid} belongs to a different workspace"
                    )})),
                ));
            }
            Some(pid.to_string())
        }
        None => None,
    };

    let saved = state
        .store
        .add_workspace_root(
            NewWorkspaceRoot {
                workspace_id: ws.id.clone(),
                path: path.to_string(),
                role: req.role,
                label: req.label,
                parent_id,
            },
            now_ms(),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;

    refresh_workspace_deps_context(&state, &id).await;

    Ok(Json(WorkspaceRoot {
        id: saved.id,
        path: saved.path,
        role: saved.role,
        label: saved.label,
        parent_id: saved.parent_id,
        branch: None, // filled on the next workspaces list refetch
    }))
}

/// `DELETE /api/workspaces/:id/roots?id=<root_id>` — detach a node from the
/// workspace's logical tree, CASCADING to all of its descendants. The node id
/// rides in the query string (DELETE has no body in the frontend's fetch).
/// Refreshes the managed context block afterwards (stripping it if this
/// removed the last node).
pub async fn delete_workspace_root_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let root_id = match params.get("id").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(p) => p.to_string(),
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing required query param 'id'"})),
            ))
        }
    };

    let n = state
        .store
        .delete_workspace_root(id.clone(), root_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;

    refresh_workspace_deps_context(&state, &id).await;

    Ok(Json(json!({"deleted": n})))
}

/// `GET /api/workspaces/:id/root-suggestions[?path=<dir>]` — scan a project
/// dir for manifest-declared LOCAL PATH dependencies (package.json file:/link:,
/// Cargo.toml path deps, go.mod replace directives, pyproject.toml uv sources)
/// and return them as attachable root suggestions. `?path=` selects which dir
/// to scan (e.g. a peer project's dir when adding a child under it); it
/// defaults to the workspace's primary `cwd`. Best-effort: parse errors and
/// missing files are swallowed — this only ever feeds an optional picker.
/// Excludes the scanned dir itself and any path already attached anywhere in
/// the workspace.
pub async fn suggest_workspace_roots_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Vec<WorkspaceRoot>> {
    let ws = match state.store.get_workspace_by_id(id.clone()).await {
        Ok(Some(ws)) => ws,
        _ => return Json(Vec::new()),
    };
    // Scan the dir named by ?path= (a specific node's project dir) or fall
    // back to the workspace's primary cwd.
    let scan_dir = params
        .get("path")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ws.cwd.trim())
        .to_string();
    let cwd = std::path::Path::new(&scan_dir);

    // Canonical cwd (used to exclude the project itself from suggestions).
    let cwd_canon = std::fs::canonicalize(cwd).ok();

    // Canonical set of already-attached roots — suggestions never repeat
    // what's mounted. We canonicalize each so a `./foo` vs `/abs/foo`
    // mismatch still dedups.
    let mut already: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    if let Ok(rows) = state.store.list_workspace_roots(id.clone()).await {
        for r in rows {
            if let Ok(c) = std::fs::canonicalize(&r.path) {
                already.insert(c);
            }
        }
    }

    // (relative-or-abs path string from the manifest, label) pairs.
    let mut candidates: Vec<(String, String)> = Vec::new();

    // package.json — dependencies / devDependencies values starting with
    // `file:` or `link:` point at a local path.
    if let Ok(txt) = std::fs::read_to_string(cwd.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            for section in ["dependencies", "devDependencies"] {
                if let Some(map) = v.get(section).and_then(|s| s.as_object()) {
                    for (name, val) in map {
                        if let Some(spec) = val.as_str() {
                            for prefix in ["file:", "link:"] {
                                if let Some(rest) = spec.strip_prefix(prefix) {
                                    candidates.push((rest.to_string(), name.clone()));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Cargo.toml — line scan for inline `path = "..."` (covers both
    // `name = { path = "..." }` and a `path = "..."` line inside a
    // `[dependencies.name]` table). Label is best-effort: the crate name
    // to the left of `=` if present, else the path basename.
    if let Ok(txt) = std::fs::read_to_string(cwd.join("Cargo.toml")) {
        for line in txt.lines() {
            let trimmed = line.trim();
            if let Some(rel) = extract_quoted_after(trimmed, "path") {
                let name = trimmed
                    .split('=')
                    .next()
                    .map(|s| s.trim().trim_matches(['{', ' ']))
                    .filter(|s| !s.is_empty() && !s.starts_with('['))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| basename_of(&rel));
                candidates.push((rel, name));
            }
        }
    }

    // go.mod — `replace <module> => <target> [version]` where the target is
    // a local path (./ ../ or absolute). Label = path basename.
    if let Ok(txt) = std::fs::read_to_string(cwd.join("go.mod")) {
        for line in txt.lines() {
            let trimmed = line.trim();
            let body = trimmed.strip_prefix("replace ").unwrap_or(trimmed);
            if let Some((_, rhs)) = body.split_once("=>") {
                if let Some(target) = rhs.split_whitespace().next() {
                    if target.starts_with("./")
                        || target.starts_with("../")
                        || std::path::Path::new(target).is_absolute()
                    {
                        candidates.push((target.to_string(), basename_of(target)));
                    }
                }
            }
        }
    }

    // pyproject.toml — under `[tool.uv.sources]`, lines of the form
    // `name = { path = "..." }`. Label = name (else basename).
    if let Ok(txt) = std::fs::read_to_string(cwd.join("pyproject.toml")) {
        let mut in_uv_sources = false;
        for line in txt.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_uv_sources = trimmed == "[tool.uv.sources]";
                continue;
            }
            if in_uv_sources {
                if let Some(rel) = extract_quoted_after(trimmed, "path") {
                    let name = trimmed
                        .split('=')
                        .next()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| basename_of(&rel));
                    candidates.push((rel, name));
                }
            }
        }
    }

    // pom.xml — Maven deps are jar coordinates (groupId/artifactId/version),
    // not local paths, so we can't read a path out of the manifest. Instead we
    // LOCATE local Maven projects on disk whose own `artifactId` matches a
    // declared dependency, covering the two common local layouts. Only runs
    // when the scanned dir actually has a pom.xml. Candidates are pushed as
    // absolute paths so they flow through the same canonicalize/exclude/dedup
    // pipeline below as every other ecosystem.
    if let Ok(pom) = std::fs::read_to_string(cwd.join("pom.xml")) {
        // (1) Multi-module reactor: each <module>REL</module> is a local
        // subdir scanDir/REL. Suggest it if scanDir/REL/pom.xml exists. Label
        // = the module's own artifactId if cheaply available, else REL.
        for rel in xml_tag_values(&pom, "module") {
            let module_dir = cwd.join(&rel);
            let module_pom = module_dir.join("pom.xml");
            if module_pom.is_file() {
                let label = std::fs::read_to_string(&module_pom)
                    .ok()
                    .and_then(|m| own_artifact_id(&m))
                    .unwrap_or(rel);
                candidates.push((module_dir.to_string_lossy().into_owned(), label));
            }
        }

        // (2) Sibling projects checked out next to this one. Collect every
        // <artifactId> referenced anywhere in the scanned pom (over-collecting
        // our own/parent/plugin ids is fine — they just won't match a real
        // sibling project, or if they do the user simply won't click). Then
        // scan the parent dir's immediate children for Maven projects whose
        // OWN artifactId is in that referenced set.
        let referenced: std::collections::HashSet<String> =
            xml_tag_values(&pom, "artifactId").into_iter().collect();
        if !referenced.is_empty() {
            if let Some(parent) = cwd.parent() {
                if let Ok(entries) = std::fs::read_dir(parent) {
                    // Bound the scan so a huge parent dir can't blow up the
                    // request — only the first 200 child dirs are considered.
                    for entry in entries.flatten().take(200) {
                        let child = entry.path();
                        if !child.is_dir() {
                            continue;
                        }
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        // Skip the scanned dir itself, hidden dirs, and the
                        // usual build/vendor noise.
                        if name.starts_with('.') || name == "target" || name == "node_modules" {
                            continue;
                        }
                        // The scanned dir itself is among these children but is
                        // excluded downstream by the cwd_canon check, so we
                        // needn't special-case it here.
                        let child_pom = child.join("pom.xml");
                        if !child_pom.is_file() {
                            continue;
                        }
                        if let Ok(child_xml) = std::fs::read_to_string(&child_pom) {
                            if let Some(aid) = own_artifact_id(&child_xml) {
                                if referenced.contains(&aid) {
                                    candidates.push((child.to_string_lossy().into_owned(), aid));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Resolve each candidate relative to cwd, canonicalize, keep only
    // existing dirs, drop the cwd itself + already-attached + dupes.
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    let mut out: Vec<WorkspaceRoot> = Vec::new();
    for (rel, label) in candidates {
        let raw = std::path::Path::new(&rel);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            cwd.join(raw)
        };
        let canon = match std::fs::canonicalize(&joined) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !canon.is_dir() {
            continue;
        }
        if Some(&canon) == cwd_canon.as_ref() {
            continue;
        }
        if already.contains(&canon) {
            continue;
        }
        if !seen.insert(canon.clone()) {
            continue;
        }
        out.push(WorkspaceRoot {
            id: String::new(),
            path: canon.to_string_lossy().into_owned(),
            role: "dependency".to_string(),
            label: Some(label),
            parent_id: None,
            branch: None, // suggestion only — not attached yet
        });
    }

    Json(out)
}

/// Pull the first `"..."`-quoted value that follows `<key>` (optionally with
/// `=`) on a single manifest line, e.g. `extract_quoted_after("foo = { path
/// = \"../bar\" }", "path")` → `Some("../bar")`. Returns `None` if the key or
/// a quoted value isn't present. Deliberately simple — these are best-effort
/// suggestion parsers, not a TOML implementation.
fn extract_quoted_after(line: &str, key: &str) -> Option<String> {
    let idx = line.find(key)?;
    let after_key = &line[idx + key.len()..];
    // Require an `=` between the key and the opening quote so we don't match
    // e.g. a `paths = [...]` array as a single path.
    let eq = after_key.find('=')?;
    let after_eq = &after_key[eq + 1..];
    let start = after_eq.find('"')? + 1;
    let rest = &after_eq[start..];
    let end = rest.find('"')?;
    let val = &rest[..end];
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// Last path component of a path string, used as a fallback dependency label.
fn basename_of(p: &str) -> String {
    std::path::Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Crude XML scan: return the trimmed inner text of every `<tag>...</tag>`
/// occurrence in `xml`. Used for Maven pom.xml `<artifactId>` and `<module>`
/// extraction. Deliberately not a real XML parser — these are best-effort
/// suggestion inputs, so namespaces, comments, and attributes are ignored.
fn xml_tag_values(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        let val = after[..end].trim();
        if !val.is_empty() {
            out.push(val.to_string());
        }
        rest = &after[end + close.len()..];
    }
    out
}

/// Extract a Maven pom's OWN `artifactId` (not its parent's). A `<parent>`
/// block carries its own `<artifactId>`; to skip it we start searching after
/// the first `</parent>` (if any), else from the start, then take the first
/// `<artifactId>...</artifactId>`.
fn own_artifact_id(xml: &str) -> Option<String> {
    let search_from = xml
        .find("</parent>")
        .map(|i| i + "</parent>".len())
        .unwrap_or(0);
    xml_tag_values(&xml[search_from..], "artifactId")
        .into_iter()
        .next()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn contestant_check_judged_by_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let (passed, _) = run_contestant_check(&cwd, "exit 0").await;
        assert!(passed, "exit 0 passes the gate");
        let (passed, _) = run_contestant_check(&cwd, "exit 3").await;
        assert!(!passed, "non-zero exit fails the gate");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn contestant_check_timeout_fails_loud_instead_of_wedging_judge() {
        // Regression: a hung check command (`sleep 30` standing in for e.g. an
        // `npm test` that accidentally starts a dev server) used to block the
        // judge on `.output()` forever. With the timebox it returns promptly
        // as a FAILED check with the evidence, and the tree is killpg'ed.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let start = std::time::Instant::now();
        let (passed, out) = run_contestant_check_with_timeout(
            &cwd,
            "sleep 30",
            std::time::Duration::from_millis(100),
        )
        .await;
        assert!(!passed, "a timed-out check is a failed check");
        assert!(out.contains("timed out"), "evidence kept for the judge: {out}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "the judge is unblocked promptly, took {:?}",
            start.elapsed()
        );
    }
}
