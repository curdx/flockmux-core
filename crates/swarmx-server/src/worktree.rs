//! Git worktree helpers for "directions" (threads) — automatic, zero-config
//! file isolation so two directions in one project don't overwrite each other.
//!
//! Design goal (per product spec): the USER never sees git/branch/worktree/
//! checkout. When a direction wants isolation we silently:
//!   1. make the project a git repo if it isn't one yet (`git init` + a first
//!      commit so `worktree add` has a base), and
//!   2. `git worktree add <project>-<branch> -b <branch>` — a sibling dir next
//!      to the project the user can see in Finder/IDE.
//!
//! All git invocations are time-boxed on a worker thread (mirrors F17's
//! `binary_supports_flag` pattern) so a hung/slow git can never stall the async
//! spawn path. We NEVER touch global git config — only `-c user.*` inline when
//! a fresh repo has no identity, so the first commit doesn't fail on a machine
//! without `git config --global user.email`.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Process-wide lock serializing the full git-isolation sequence (init + first
/// commit + worktree add) against ONE repo. Concurrent isolation of several
/// directions in the same project (notably a fusion fan-out: N contestants
/// isolating at once) race on the repo's `.git/config` / index / HEAD locks —
/// the first wins and the rest fail with "could not lock config file", silently
/// degrading to shared/unisolated. The git calls are fast and already
/// 30s-timeboxed inside `git()`, so a global serialization point is cheap
/// insurance. NOT keyed per-repo: the simplest correct thing, contention is
/// negligible at human scale.
static GIT_ISOLATION_LOCK: Mutex<()> = Mutex::new(());

/// Run the full isolation sequence (ensure repo + first commit, then add a
/// worktree on `branch`) under the process-wide isolation lock, so concurrent
/// callers against the same repo don't race on git's on-disk locks. This is the
/// entry point background isolation should use (see `spawn_thread_worktree`).
pub fn isolate_into_worktree(repo_cwd: &Path, branch: &str) -> Result<PathBuf> {
    let _guard = GIT_ISOLATION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    git_init_with_commit(repo_cwd)?;
    worktree_add(repo_cwd, branch)
}

/// Hard ceiling for any single git invocation. `worktree add` on a large repo
/// can take a couple seconds; init/commit are fast. 30s is generous headroom
/// while still bounding a pathological hang.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const SWARMX_GIT_USER_NAME: &str = "user.name=swarmx";
const SWARMX_GIT_USER_EMAIL: &str = "user.email=swarmx@localhost";

/// Output of a finished git command we care about.
struct GitOut {
    status_ok: bool,
    stdout: String,
    stderr: String,
}

/// Run `git -C <cwd> <args...>` with a hard timeout on a worker thread, so a
/// hung git can't block the caller (the async spawn path calls this via
/// `spawn_blocking`). Returns `Err` only when git couldn't be launched or the
/// timeout elapsed; a non-zero exit is reported via `GitOut.status_ok=false`.
fn git(cwd: &Path, args: &[&str]) -> Result<GitOut> {
    let cwd = cwd.to_path_buf();
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // output() drains stdout+stderr (so git can't deadlock on a full pipe).
        let out = crate::runtime_path::tool_command("git")
            .arg("-C")
            .arg(&cwd)
            .args(&owned)
            .output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(GIT_TIMEOUT) {
        Ok(Ok(o)) => Ok(GitOut {
            status_ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(anyhow!("git spawn failed: {e}")),
        Err(_) => Err(anyhow!("git command timed out after {GIT_TIMEOUT:?}")),
    }
}

/// Run a git command with a local, throwaway identity. Use this for commands
/// that may create a commit (`commit`, non-fast-forward `merge`) so clean CI
/// runners and fresh user machines do not need global git config.
fn git_with_swarmx_identity(cwd: &Path, args: &[&str]) -> Result<GitOut> {
    let mut git_args = Vec::with_capacity(args.len() + 4);
    git_args.extend_from_slice(&["-c", SWARMX_GIT_USER_NAME, "-c", SWARMX_GIT_USER_EMAIL]);
    git_args.extend_from_slice(args);
    git(cwd, &git_args)
}

/// Is `dir` inside a git work tree? Used to decide whether a direction can get
/// a worktree immediately or whether we need to `git init` first. A timeout /
/// launch error is treated as "not a repo" (we'll try to init, which is safe —
/// `git init` on an existing repo is a no-op).
#[allow(dead_code)] // documented helper; current flow uses idempotent init
pub fn is_git_repo(dir: &Path) -> bool {
    match git(dir, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(o) => o.status_ok && o.stdout.trim() == "true",
        Err(_) => false,
    }
}

/// The branch currently checked out in `dir`, for the sidebar's live branch
/// chip. Returns `None` for a detached HEAD (rev-parse yields the literal
/// "HEAD"), a non-git dir, or any git error/timeout — the caller renders no
/// chip in that case. Blocking (shells out to git); call off the async path.
pub fn current_branch(dir: &Path) -> Option<String> {
    let o = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    if !o.status_ok {
        return None;
    }
    let b = o.stdout.trim();
    if b.is_empty() || b == "HEAD" {
        return None; // detached / no commit yet
    }
    Some(b.to_string())
}

/// Does `dir`'s work tree have uncommitted changes — staged, unstaged, or
/// untracked? `git status --porcelain` prints one line per change, so any
/// non-empty output means dirty. This is **read-only** (never touches the work
/// tree or index), so it's safe to call while an agent is mid-edit — surfacing
/// "this direction has unsaved work" without disturbing it. Returns `false` for
/// a non-git dir / error / timeout: we'd rather show no marker than a false
/// "dirty". Blocking (shells out to git); call off the async path.
pub fn working_dirty(dir: &Path) -> bool {
    match git(dir, &["status", "--porcelain"]) {
        Ok(o) => o.status_ok && !o.stdout.trim().is_empty(),
        Err(_) => false,
    }
}

/// Make `dir` a git repo and create an initial commit so `worktree add` has a
/// base to branch from. Idempotent-ish: `git init` on an existing repo is a
/// no-op; if there's already a commit we skip committing. Uses inline
/// `-c user.*` ONLY when the repo has no identity, so a machine without a
/// global git identity can still get its first commit — global config is never
/// modified.
pub fn git_init_with_commit(dir: &Path) -> Result<()> {
    // 1) init (no-op if already a repo).
    let init = git(dir, &["init"]).context("git init")?;
    if !init.status_ok {
        return Err(anyhow!("git init failed: {}", init.stderr.trim()));
    }

    // 2) already have a commit? then we're done — don't churn history.
    if git(dir, &["rev-parse", "--verify", "HEAD"])
        .map(|o| o.status_ok)
        .unwrap_or(false)
    {
        return Ok(());
    }

    // 3) stage everything. Empty dir is fine (commit --allow-empty below).
    //    A first commit on a REAL user project would otherwise sweep
    //    node_modules/target/dist (hundreds of MB of build artifacts) into git
    //    when the dir has no .gitignore — silently, with no prompt. Exclude the
    //    usual heavyweight artifact dirs via the repo-LOCAL info/exclude (never
    //    the user's tracked .gitignore) before the full stage; info/exclude only
    //    hides UNTRACKED files, so anything the user deliberately tracks later
    //    is unaffected. The common-dir exclude also covers every direction
    //    worktree spawned from this repo.
    ignore_paths_locally(dir, &["node_modules/", "target/", "dist/"]);
    let add = git(dir, &["add", "-A"]).context("git add -A")?;
    if !add.status_ok {
        return Err(anyhow!("git add failed: {}", add.stderr.trim()));
    }

    // 4) commit. Supply an inline identity so the commit doesn't fail on a box
    //    with no `git config --global user.email`. `--allow-empty` covers a
    //    brand-new empty project dir.
    let commit = git_with_swarmx_identity(
        dir,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "swarmx: initial commit (enables parallel directions)",
        ],
    )
    .context("git commit")?;
    if !commit.status_ok {
        return Err(anyhow!("git commit failed: {}", commit.stderr.trim()));
    }
    Ok(())
}

/// W1-2: capture a direction worktree's UNCOMMITTED work as a commit on its
/// branch, so `merge_into_base` (which brings only COMMITTED content via
/// `git merge`) doesn't silently drop it. Workers edit files in the isolated
/// direction worktree but are never told to `git commit`; without this, merge
/// reports success while merging an empty/stale branch — real data loss plus a
/// lying "Merged" status (confirmed by deterministic repro). No-op when the
/// worktree is clean. Returns `Ok(true)` if it created a commit.
///
/// Caller MUST only pass an ISOLATED direction worktree here (never the base /
/// main project), since this does `git add -A`: the direction worktree is
/// swarmx-managed and only holds worker output, whereas the base may carry the
/// user's own uncommitted edits that we must never touch.
pub fn commit_worktree_work(worktree_cwd: &Path, message: &str) -> Result<bool> {
    if !working_dirty(worktree_cwd) {
        return Ok(false);
    }
    let add = git(worktree_cwd, &["add", "-A"]).context("git add -A")?;
    if !add.status_ok {
        return Err(anyhow!("git add -A failed: {}", add.stderr.trim()));
    }
    let commit = git_with_swarmx_identity(worktree_cwd, &["commit", "-m", message])
        .context("git commit")?;
    if !commit.status_ok {
        return Err(anyhow!("git commit failed: {}", commit.stderr.trim()));
    }
    Ok(true)
}

/// Sanitize a human/AI-suggested branch name into the dir suffix we append to
/// the project basename. Git branch names are fairly permissive but a sibling
/// DIRECTORY name should be filesystem-friendly: keep `[a-z0-9._-]`, collapse
/// the rest to `-`, trim leading/trailing separators, lowercase. Empty → "dir".
/// Reused by the thread REST layer to derive a stable blackboard slug + branch
/// from a human/AI-supplied direction name.
pub(crate) fn sanitize_suffix(branch: &str) -> String {
    let mut s: String = branch
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s
        .trim_matches(|c| c == '-' || c == '.' || c == '_')
        .to_string();
    if s.is_empty() {
        // A fully non-ASCII name (e.g. Chinese "深色模式") would otherwise
        // collapse to the SAME literal for every such name, so distinct
        // directions would collide on one branch/worktree (only kept apart by
        // unique_thread_slug's `-2/-3` suffixing) and read as meaningless.
        // Derive a stable short hash of the original so each distinct name maps
        // to its own slug/branch. DefaultHasher has a fixed (seedless) initial
        // state, so this is deterministic across runs.
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        branch.hash(&mut h);
        format!("dir-{:06x}", (h.finish() as u32) & 0x00ff_ffff)
    } else {
        s
    }
}

/// Compute the sibling worktree path for a project + branch: `<parent>/<base>-<suffix>`.
/// Pure (no IO) so it's unit-testable and the caller can show a path preview.
pub fn worktree_dest(project_cwd: &Path, branch: &str) -> PathBuf {
    let parent = project_cwd.parent().unwrap_or_else(|| Path::new("."));
    let base = project_cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    parent.join(format!("{base}-{}", sanitize_suffix(branch)))
}

/// Create a git worktree for `branch` as a sibling of `repo_cwd`. Returns the
/// destination path on success. If `branch` already exists, retries without
/// `-b` (attach the existing branch instead of creating it). The caller has
/// already ensured `repo_cwd` is a git repo (via `git_init_with_commit`).
pub fn worktree_add(repo_cwd: &Path, branch: &str) -> Result<PathBuf> {
    let dest = worktree_dest(repo_cwd, branch);
    let dest_str = dest.to_string_lossy().into_owned();

    // First try creating a fresh branch.
    let with_new = git(repo_cwd, &["worktree", "add", &dest_str, "-b", branch])
        .context("git worktree add -b")?;
    if with_new.status_ok {
        return Ok(dest);
    }

    // Branch may already exist → attach it instead of -b. (Also covers a stale
    // dest dir from a prior run; surfaced via stderr if it still fails.)
    let attach = git(repo_cwd, &["worktree", "add", &dest_str, branch])
        .context("git worktree add (existing branch)")?;
    if attach.status_ok {
        return Ok(dest);
    }

    Err(anyhow!(
        "git worktree add failed for branch `{branch}` at {dest_str}: {} / {}",
        with_new.stderr.trim(),
        attach.stderr.trim()
    ))
}

/// Local branches of the repo at `dir`, each flagged whether it's currently
/// checked out in some worktree (the main one or a direction's). A checked-out
/// branch can't be attached to a new worktree, so the "open existing branch"
/// picker disables those. Empty for a non-git dir / error / timeout. Blocking
/// (shells out to git); call off the async path.
pub fn list_branches(dir: &Path) -> Vec<(String, bool)> {
    let names = match git(dir, &["branch", "--format=%(refname:short)"]) {
        Ok(o) if o.status_ok => o.stdout,
        _ => return Vec::new(),
    };
    // Branches checked out in any worktree — `worktree list --porcelain` emits a
    // `branch refs/heads/<name>` line per attached worktree.
    let mut checked_out: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(o) = git(dir, &["worktree", "list", "--porcelain"]) {
        if o.status_ok {
            for line in o.stdout.lines() {
                if let Some(b) = line.strip_prefix("branch refs/heads/") {
                    checked_out.insert(b.trim().to_string());
                }
            }
        }
    }
    names
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|name| (name.to_string(), checked_out.contains(name)))
        .collect()
}

/// Remove a worktree (best-effort — caller logs but doesn't fail the request).
/// `--force` so an uncommitted-changes worktree still gets cleaned up when the
/// user deletes the direction (they chose to discard it).
pub fn worktree_remove(repo_cwd: &Path, dest: &Path) -> Result<()> {
    let dest_str = dest.to_string_lossy().into_owned();
    let out = git(repo_cwd, &["worktree", "remove", "--force", &dest_str])
        .context("git worktree remove")?;
    if out.status_ok {
        Ok(())
    } else {
        Err(anyhow!("git worktree remove failed: {}", out.stderr.trim()))
    }
}

/// Commits `branch` is ahead of / behind `base`, computed **purely locally**
/// (never fetches). Returns `(ahead, behind)` where `ahead` = commits on
/// `branch` not on `base`, `behind` = commits on `base` not on `branch`.
///
/// `git rev-list --left-right --count <base>...<branch>` prints
/// "`<behind>`\t`<ahead>`" (left = reachable from base only; right = branch
/// only). Run from the main repo — refs are shared across worktrees, so it sees
/// every direction's branch. `None` on any git error, or when `base == branch`
/// (the main direction is its own base — ahead/behind is meaningless there).
pub fn ahead_behind(repo_cwd: &Path, base: &str, branch: &str) -> Option<(i64, i64)> {
    if base == branch || base.is_empty() || branch.is_empty() {
        return None;
    }
    let spec = format!("{base}...{branch}");
    let out = git(repo_cwd, &["rev-list", "--left-right", "--count", &spec]).ok()?;
    if !out.status_ok {
        return None;
    }
    let mut it = out.stdout.split_whitespace();
    let behind: i64 = it.next()?.parse().ok()?;
    let ahead: i64 = it.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// Delete a local branch (best-effort). `-D` (force) so it works whether or not
/// the branch is fully merged — a direction delete is a "discard" (the user
/// chose to drop it), and a merge-then-cleanup deletes an already-merged branch
/// either way. MUST be called AFTER the branch's worktree is removed: git
/// refuses to delete a branch still checked out in a worktree. No-op /
/// best-effort on any error (the branch lingering is harmless vs failing the
/// delete). Without this, a same-named direction recreated later would re-attach
/// this stale branch's history.
pub fn delete_branch(repo_cwd: &Path, branch: &str) -> Result<()> {
    let out = git(repo_cwd, &["branch", "-D", branch]).context("git branch -D")?;
    if out.status_ok {
        Ok(())
    } else {
        Err(anyhow!("git branch -D failed: {}", out.stderr.trim()))
    }
}

/// Make swarmx's own managed artifacts invisible to git by adding them to the
/// repo's LOCAL `.git/info/exclude` — never the user's tracked `.gitignore`.
///
/// We drop `.claude/settings.local.json` (the Stop-hook config) and
/// `.codex/hooks.json` into the project cwd AND every direction worktree at
/// spawn time. Untracked, they otherwise show as changes — falsely marking the
/// tree "dirty" (bogus sidebar dot) and blocking "merge to main" (which refuses
/// a dirty base). `info/exclude` only hides UNTRACKED files, so a user who
/// actually tracks these keeps them. It lives in the shared common dir, so one
/// call covers the main worktree and all direction worktrees. Idempotent;
/// best-effort no-op for a non-git dir.
pub fn ignore_managed_artifacts(repo_cwd: &Path) {
    ignore_paths_locally(
        repo_cwd,
        &[
            ".claude/settings.local.json",
            ".codex/hooks.json",
            // The swarm MCP config we write per-agent: reasonix's root `.mcp.json`
            // and zulu's `.comate/` / kimi's `.kimi-code/` (kernel/config dirs).
            // These are scaffolding, NOT the
            // agent's work — if they leak into git they get committed by
            // contestants/the judge and merged into base, and they falsely read as
            // "the agent produced work" in the fusion completion checks.
            ".mcp.json",
            ".comate/",
            ".kimi-code/",
        ],
    );
}

/// Append `patterns` to the repo's LOCAL `.git/info/exclude` (idempotent),
/// resolving the shared common dir so it covers the main worktree + every
/// direction worktree. Used to hide swarmx-generated files (managed hook
/// config, and a deps-context CLAUDE.md/AGENTS.md we created) from git's "dirty"
/// accounting — `info/exclude` only affects UNTRACKED files, so anything the
/// user actually tracks is unaffected. Best-effort no-op for a non-git dir.
///
/// CALLER CONTRACT for CLAUDE.md/AGENTS.md: only pass these when swarmx
/// authored the WHOLE file (it didn't exist / was empty before our block) —
/// never when appending our block to a user's existing file, or we'd hide their
/// real context file.
pub fn ignore_paths_locally(repo_cwd: &Path, patterns: &[&str]) {
    let common = match git(repo_cwd, &["rev-parse", "--git-common-dir"]) {
        Ok(o) if o.status_ok && !o.stdout.trim().is_empty() => o.stdout.trim().to_string(),
        _ => return,
    };
    // rev-parse can return a relative path (e.g. ".git"); resolve against cwd.
    let common_path = {
        let p = Path::new(&common);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            repo_cwd.join(p)
        }
    };
    let exclude = common_path.join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let missing: Vec<&str> = patterns
        .iter()
        .copied()
        .filter(|pat| !existing.lines().any(|l| l.trim() == *pat))
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# swarmx-managed (auto-excluded; not committed)\n");
    for pat in missing {
        out.push_str(pat);
        out.push('\n');
    }
    if let Some(dir) = exclude.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&exclude, out);
}

// ── merge a direction back into the main line ───────────────────────────────

/// What a [`merge_into_base`] attempt did.
#[derive(Debug)]
pub enum MergeOutcome {
    /// Merged cleanly. `files` = how many files the direction had changed.
    Clean { files: usize },
    /// The merge left conflict markers in these (repo-relative) files; the work
    /// tree is mid-merge (MERGE_HEAD set) awaiting resolution by a human/agent.
    Conflict { files: Vec<String> },
    /// The merge couldn't run at all (git error / timeout / nothing to merge).
    Error { msg: String },
}

/// Repo-relative files the `from` branch changed relative to its merge-base with
/// `base` — i.e. "what this direction actually did". Uses three-dot
/// `git diff --name-only <base>...<from>` so unrelated churn on `base` since the
/// branch point isn't counted. Empty on any git error/timeout. Blocking; call
/// off the async path.
pub fn diff_summary(repo_cwd: &Path, base: &str, from: &str) -> Vec<String> {
    let spec = format!("{base}...{from}");
    match git(repo_cwd, &["diff", "--name-only", &spec]) {
        Ok(o) if o.status_ok => o
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Repo-relative files with unresolved merge conflicts
/// (`git diff --name-only --diff-filter=U`). Non-empty only while a merge is in
/// progress and stuck on conflicts. Blocking; call off the async path.
pub fn conflicted_files(repo_cwd: &Path) -> Vec<String> {
    match git(repo_cwd, &["diff", "--name-only", "--diff-filter=U"]) {
        Ok(o) if o.status_ok => o
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Merge `from` into the branch currently checked out at `repo_cwd`. `repo_cwd`
/// MUST be the project's PRIMARY worktree (the workspace cwd, on `base`) — never
/// an isolated direction worktree, which can't hold the merge. Runs
/// `git merge --no-edit <from>`; on conflict the work tree is left mid-merge
/// (MERGE_HEAD + conflict markers) for a resolver to finish.
///
/// Caller MUST ensure `repo_cwd` is clean first (`working_dirty` == false): git
/// refuses to merge over uncommitted changes and we never stash silently.
pub fn merge_into_base(repo_cwd: &Path, base: &str, from: &str) -> MergeOutcome {
    // Count the direction's changed files BEFORE merging — afterwards `from` is
    // an ancestor of `base` and the three-dot diff would be empty.
    let changed = diff_summary(repo_cwd, base, from).len();
    let out = match git_with_swarmx_identity(repo_cwd, &["merge", "--no-edit", from]) {
        Ok(o) => o,
        Err(e) => return MergeOutcome::Error { msg: e.to_string() },
    };
    if out.status_ok {
        return MergeOutcome::Clean { files: changed };
    }
    // Non-zero exit: a real conflict leaves unmerged paths + MERGE_HEAD; anything
    // else (e.g. "merge is not possible because you have unmerged files", bad
    // ref) is a hard error we surface verbatim.
    let conflicts = conflicted_files(repo_cwd);
    if !conflicts.is_empty() {
        MergeOutcome::Conflict { files: conflicts }
    } else {
        let msg = if out.stderr.trim().is_empty() {
            out.stdout.trim().to_string()
        } else {
            out.stderr.trim().to_string()
        };
        MergeOutcome::Error { msg }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn sanitize_suffix_makes_fs_friendly_names() {
        assert_eq!(sanitize_suffix("dark-mode"), "dark-mode");
        assert_eq!(sanitize_suffix("Dark Mode!!"), "dark-mode");
        assert_eq!(sanitize_suffix("feature/api v2"), "feature-api-v2");
        // Non-ascii names no longer all collapse to a single "dir": each gets a
        // stable, distinct `dir-<hash>` so directions don't share a branch.
        assert!(sanitize_suffix("深色").starts_with("dir-"));
        assert_eq!(sanitize_suffix("深色"), sanitize_suffix("深色")); // deterministic
        assert_ne!(sanitize_suffix("深色"), sanitize_suffix("浅色")); // distinct names → distinct slugs
        assert_eq!(sanitize_suffix("深色 mode"), "mode"); // ascii kept when present
        assert_eq!(sanitize_suffix("--x--"), "x");
        assert!(sanitize_suffix("").starts_with("dir-"));
    }

    #[test]
    fn worktree_dest_is_sibling_of_project() {
        let p = worktree_dest(Path::new("/home/me/code/myproj"), "dark-mode");
        assert_eq!(p, Path::new("/home/me/code/myproj-dark-mode"));
    }

    #[test]
    fn worktree_dest_handles_trailing_slash_and_messy_branch() {
        let p = worktree_dest(Path::new("/tmp/proj"), "Feature/API v2");
        assert_eq!(p, Path::new("/tmp/proj-feature-api-v2"));
    }

    // End-to-end git test: init a throwaway repo, add a worktree, remove it.
    // Skipped automatically if `git` isn't on PATH.
    #[test]
    fn init_add_remove_roundtrip_on_real_git() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // no git → skip
        }
        let dir = tempfile::TempDir::new().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("README.md"), b"hi").unwrap();

        assert!(!is_git_repo(&proj), "fresh dir is not a repo");
        assert_eq!(current_branch(&proj), None, "non-repo has no branch");
        git_init_with_commit(&proj).expect("init+commit");
        assert!(is_git_repo(&proj), "now a repo");
        // The default branch name varies (main/master) by git version/config,
        // so just assert we read *some* non-empty branch on the committed repo.
        assert!(
            current_branch(&proj).is_some_and(|b| !b.is_empty()),
            "committed repo reports a branch",
        );

        let wt = worktree_add(&proj, "dark-mode").expect("worktree add");
        assert!(wt.exists(), "worktree dir created");
        assert!(wt.join("README.md").exists(), "worktree has the files");
        assert_eq!(wt.file_name().unwrap(), "proj-dark-mode");
        assert_eq!(
            current_branch(&wt).as_deref(),
            Some("dark-mode"),
            "worktree is on its own branch",
        );

        worktree_remove(&proj, &wt).expect("worktree remove");
        assert!(!wt.exists(), "worktree dir gone after remove");

        // After the worktree is gone, the branch can be deleted (it's no longer
        // checked out anywhere). list_branches should then not include it.
        assert!(
            list_branches(&proj).iter().any(|(b, _)| b == "dark-mode"),
            "branch present before delete",
        );
        delete_branch(&proj, "dark-mode").expect("delete branch");
        assert!(
            !list_branches(&proj).iter().any(|(b, _)| b == "dark-mode"),
            "branch gone after delete",
        );
    }

    // Real-git merge: clean (new file) then conflict (same file two ways).
    // Skipped automatically if `git` isn't on PATH.
    #[test]
    fn merge_clean_then_conflict_on_real_git() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // no git → skip
        }
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        // Run git with an inline identity so commits don't fail on a machine
        // without global user.* (mirrors git_init_with_commit).
        let commit = |repo: &Path, msg: &str| {
            git(repo, &["add", "-A"]).unwrap();
            git(
                repo,
                &[
                    "-c",
                    "user.email=t@t.t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "-m",
                    msg,
                ],
            )
            .unwrap();
        };
        git(&repo, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("a.txt"), "base\n").unwrap();
        commit(&repo, "base");
        let base = current_branch(&repo).expect("base branch");

        // feature: adds a new file → clean merge.
        git(&repo, &["checkout", "-q", "-b", "feature"]).unwrap();
        std::fs::write(repo.join("b.txt"), "feature\n").unwrap();
        commit(&repo, "feat");
        git(&repo, &["checkout", "-q", &base]).unwrap();

        assert_eq!(
            diff_summary(&repo, &base, "feature"),
            vec!["b.txt".to_string()]
        );
        match merge_into_base(&repo, &base, "feature") {
            MergeOutcome::Clean { files } => assert_eq!(files, 1),
            other => panic!("expected clean, got {other:?}"),
        }

        // feature2: edits a.txt; base edits a.txt differently → conflict.
        git(&repo, &["checkout", "-q", "-b", "feature2"]).unwrap();
        std::fs::write(repo.join("a.txt"), "from-feature2\n").unwrap();
        commit(&repo, "f2");
        git(&repo, &["checkout", "-q", &base]).unwrap();
        std::fs::write(repo.join("a.txt"), "from-base\n").unwrap();
        commit(&repo, "base2");

        match merge_into_base(&repo, &base, "feature2") {
            MergeOutcome::Conflict { files } => {
                assert!(files.contains(&"a.txt".to_string()), "a.txt conflicted");
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        assert!(
            !conflicted_files(&repo).is_empty(),
            "mid-merge has conflicts"
        );
        let _ = git(&repo, &["merge", "--abort"]); // tidy up the in-progress merge
    }

    #[test]
    fn ahead_behind_counts_diverged_commits() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // no git → skip
        }
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        let commit = |repo: &Path, file: &str, body: &str, msg: &str| {
            std::fs::write(repo.join(file), body).unwrap();
            git(repo, &["add", "-A"]).unwrap();
            git(
                repo,
                &[
                    "-c",
                    "user.email=t@t.t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "-m",
                    msg,
                ],
            )
            .unwrap();
        };
        git(&repo, &["init", "-q"]).unwrap();
        commit(&repo, "a.txt", "base\n", "base");
        let base = current_branch(&repo).unwrap();

        // feat diverges: +2 commits of its own, base gains +1 → ahead 2, behind 1.
        git(&repo, &["checkout", "-q", "-b", "feat"]).unwrap();
        commit(&repo, "b.txt", "1\n", "feat1");
        commit(&repo, "b.txt", "2\n", "feat2");
        git(&repo, &["checkout", "-q", &base]).unwrap();
        commit(&repo, "a.txt", "base2\n", "base2");

        assert_eq!(
            ahead_behind(&repo, &base, "feat"),
            Some((2, 1)),
            "ahead 2, behind 1"
        );
        // base vs itself → None (main direction is its own base).
        assert_eq!(ahead_behind(&repo, &base, &base), None);
        // No divergence (fresh branch off base) → (0, 0).
        git(&repo, &["branch", "fresh", &base]).unwrap();
        assert_eq!(ahead_behind(&repo, &base, "fresh"), Some((0, 0)));
    }

    // The first commit on a non-git project dir must NOT sweep heavyweight
    // build artifacts (node_modules/target/dist) into git — they're excluded
    // via the repo-local info/exclude before the full `git add -A`.
    #[test]
    fn init_with_commit_excludes_heavy_artifact_dirs() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // no git → skip
        }
        let dir = tempfile::TempDir::new().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join("node_modules/dep")).unwrap();
        std::fs::create_dir_all(proj.join("target/debug")).unwrap();
        std::fs::create_dir_all(proj.join("dist")).unwrap();
        std::fs::write(proj.join("README.md"), b"hi").unwrap();
        std::fs::write(proj.join("node_modules/dep/index.js"), b"x").unwrap();
        std::fs::write(proj.join("target/debug/app"), b"x").unwrap();
        std::fs::write(proj.join("dist/bundle.js"), b"x").unwrap();

        git_init_with_commit(&proj).expect("init+commit");

        let tracked = git(&proj, &["ls-files"]).unwrap();
        assert!(tracked.status_ok);
        assert!(
            tracked.stdout.contains("README.md"),
            "real source file committed: {}",
            tracked.stdout
        );
        for heavy in ["node_modules/", "target/", "dist/"] {
            assert!(
                !tracked.stdout.contains(heavy),
                "{heavy} must not be committed: {}",
                tracked.stdout
            );
        }
        let exclude = std::fs::read_to_string(proj.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("node_modules/"), "exclude written");
    }

    #[test]
    fn ignore_managed_artifacts_writes_patterns_idempotently() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // no git → skip
        }
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();

        ignore_managed_artifacts(&repo);
        let exclude = repo.join(".git/info/exclude");
        let body = std::fs::read_to_string(&exclude).unwrap();
        assert!(
            body.contains(".claude/settings.local.json"),
            "claude pattern added"
        );
        assert!(body.contains(".codex/hooks.json"), "codex pattern added");
        // The swarm MCP configs must be excluded so they don't leak into git /
        // read as "the agent produced work".
        assert!(body.contains(".mcp.json"), "reasonix root .mcp.json excluded");
        assert!(body.contains(".comate/"), "zulu .comate/ excluded");
        assert!(body.contains(".kimi-code/"), "kimi .kimi-code/ excluded");

        // Idempotent: a second call must not duplicate the entries.
        ignore_managed_artifacts(&repo);
        let body2 = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(
            body2.matches(".claude/settings.local.json").count(),
            1,
            "no duplicate exclude entry on re-run",
        );
    }
}
