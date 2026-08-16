//! swarmx-server: axum-based HTTP/WS gateway.
//!
//! M1 surface:
//!   GET  /api/plugins          — list available CLI plugins
//!   POST /api/agent            — spawn an agent ({ cli: "claude"|"codex" })
//!   DELETE /api/agent/:id      — kill an agent
//!   WS   /ws/pty/:id           — bidirectional PTY bridge
//!
//! Bind: 127.0.0.1:7777 (loopback only, single-user local). There is no
//! token auth yet, but `require_local_origin` rejects any request carrying a
//! non-local `Origin` header — this is what stops a random web page the user
//! visits from driving their agents over a cross-site WebSocket (WS bypasses
//! CORS, so the CORS layer alone is not a security boundary).

mod agent_lifecycle;
mod billing;
// Per-CLI behavior adapters (claude / codex / opencode) behind one CliAdapter
// trait — every CLI-specific spawn seam lives in its own `cli/<name>.rs`, so the
// generic spawn pipeline never branches on a CLI id. See `cli/mod.rs`.
mod cli;
mod cron;
mod engine_probe;
mod models_config;
mod opencode_tui;
mod reasonix_serve;
mod zulu_serve;
mod comate;
mod fusion;
mod input_delivery;
mod plugins;
mod pty_query;
mod pty_stream;
mod reaper;
mod registry;
mod roles;
// W2-1 verification gate: security-hardened allowlist + timeboxed process-group
// exec. Wired into the handoff path (role manifest `done_checks` → spawn
// validation → exit-key gate before auto-kill) — see
// docs/w2-1-verification-gate-design-2026-06-15.md.
mod verify;
mod routes;
mod runtime_path;
mod spawn;
mod spells;
mod tokens;
mod transcript;
mod wake;
mod wake_claim;
// Git worktree helpers for thread isolation, wired into the thread REST
// handlers (rename → background `worktree add`). `is_git_repo` stays as a
// documented helper even though the current flow relies on idempotent init.
mod worktree;

use anyhow::{Context, Result};
use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use swarmx_storage::Store;
use swarmx_swarm::{Swarm, WatcherHandle};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt; // ServeDir::oneshot in the path-aware SPA fallback
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub plugins: Arc<plugins::PluginRegistry>,
    /// Loaded spell manifests. Optional — a fresh checkout may have no
    /// `spells/` directory and the server still starts. Spells are read
    /// once at startup; no hot-reload (each spell run is a fresh read of
    /// in-memory state).
    pub spells: Arc<spells::SpellRegistry>,
    /// Loaded role manifests from `roles/`. Same load-once-no-hot-reload
    /// semantics as spells. Empty registry is fine; spells without
    /// `role_ref` work without any roles loaded.
    pub roles: Arc<roles::RoleRegistry>,
    /// F1 per-CLI model config: maps abstract tiers → concrete model ids per
    /// CLI (user-editable via the 模型 settings page / `/api/models`). Read at
    /// the spawn chokepoint to turn a role/spawn tier into the right concrete
    /// model for whichever CLI is launching. `RwLock` so a PUT can hot-swap it.
    pub models: Arc<tokio::sync::RwLock<models_config::ModelConfig>>,
    pub registry: registry::Registry,
    pub shim_path: PathBuf,
    /// Absolute path to the `swarmx-mcp` binary. Baked into per-spawn
    /// MCP config entries so claude / codex launch the swarm bridge with
    /// no PATH lookup.
    pub mcp_bin: PathBuf,
    /// Base URL of this server's own REST API (loopback only today).
    /// Stamped into MCP entries so the subprocess knows where to talk.
    pub server_url: String,
    pub workspaces_root: PathBuf,
    pub store: Arc<Store>,
    pub swarm: Arc<Swarm>,
    pub blackboard_root: PathBuf,
    pub recordings_root: PathBuf,
    /// Keeps the notify-debouncer alive for the program's lifetime. Wrapped
    /// in `Arc` so `AppState` stays `Clone`. Drop terminates the watcher.
    pub _watcher: Arc<WatcherHandle>,
    /// M6b: per-agent `depends_on` table. `agent_id → blackboard keys this
    /// agent is waiting on`. Populated at spell launch, cleaned up at
    /// agent kill. The `WakeCoordinator` background task consults this on
    /// every `SwarmEvent::BlackboardChanged` and proactively wakes
    /// matching subscribers.
    pub wake_subs: wake::WakeSubs,
    /// M6c step 5: per-agent expected handoff signal. `agent_id → key
    /// this agent is *supposed* to write before it exits`. The
    /// `WakeCoordinator` watches `SwarmEvent::AgentState{Exited}` and,
    /// when the agent dies without producing this key, synthesizes a
    /// `<key>.error` write so downstream dependents can branch into
    /// their upstream-failed path instead of waiting forever.
    pub exit_keys: wake::ExitKeys,
    /// Bounds concurrent throwaway one-shot CLI queries (`/api/prompt/optimize`,
    /// `/api/blackboard/compact`). Each spawns a real `claude` over PTY *outside*
    /// the `max_live_agents` bookkeeping, so without a cap a local caller looping
    /// either endpoint could fork unbounded CLIs (PTY/RAM/FD exhaustion + quota
    /// burn). A small independent budget bounds them without starving real agents.
    pub oneshot_limiter: Arc<tokio::sync::Semaphore>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // File logging: a daily-rolled log under ~/.swarmx/logs so a crash leaves
    // a trail beyond the terminal. The non-blocking writer's guard must outlive
    // the program — `_log_guard` holds it for `main`'s whole duration.
    let logs_dir = log_dir_default();
    let _ = std::fs::create_dir_all(&logs_dir);
    let file_appender = tracing_appender::rolling::daily(&logs_dir, "swarmx.log");
    let (file_writer, _log_guard) = tracing_appender::non_blocking(file_appender);

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,swarmx=debug".into());
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false),
        )
        .init();

    // Subcommand: `swarmx-server doctor` runs a preflight self-check + exits.
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        return run_doctor().await;
    }

    // L5a: layered registry — bundled `cli-plugins/` first, then the user
    // override layer `~/.swarmx/cli-plugins/` (last-writer-wins by id). Lets
    // a user customize or add a CLI without forking the repo.
    let plugins_dir = plugins::default_plugins_dir();
    let mut plugin_layers = vec![plugins_dir.clone()];
    if let Some(user_dir) = plugins::user_plugins_dir() {
        plugin_layers.push(user_dir);
    }
    info!(layers = ?plugin_layers, "loading cli plugins (bundled + user override)");
    let plugin_registry = plugins::PluginRegistry::load_layered(&plugin_layers);
    info!(count = plugin_registry.list().len(), "plugins loaded");

    // Two-layer spell registry: built-ins compiled into the binary (so a
    // packaged app with no reachable `spells/` dir still ships the `init`
    // spell that every workspace-create / orchestrator-spawn drives) overlaid
    // by the repo's `spells/` dir (dev override by name). Same pattern as the
    // role registry below.
    let spells_dir = spells::default_spells_dir();
    info!(dir = %spells_dir.display(), "loading spells (builtin + dir overlay)");
    let mut spell_registry = spells::SpellRegistry::builtin();
    match spells::SpellRegistry::load_dir(&spells_dir) {
        Ok(dir_spells) => spell_registry.overlay(dir_spells),
        Err(e) => {
            tracing::warn!(?e, dir = %spells_dir.display(), "spells dir overlay failed; using builtins only")
        }
    }
    info!(count = spell_registry.list().len(), "spells loaded");

    // Two-layer role registry: built-ins compiled into the binary (so a
    // deployed server with no `roles/` dir still works) overlaid by the repo's
    // `roles/` dir (dev override by slug). Per-workspace `.swarmx/roles/`
    // overlay happens at spawn time, where the workspace cwd is known.
    let roles_dir = roles::default_roles_dir();
    info!(dir = %roles_dir.display(), "loading roles (builtin + dir overlay)");
    let mut role_registry = roles::RoleRegistry::builtin();
    match roles::RoleRegistry::load_dir(&roles_dir) {
        Ok(dir_roles) => role_registry.overlay(dir_roles),
        Err(e) => {
            tracing::warn!(?e, dir = %roles_dir.display(), "roles dir overlay failed; using builtins only")
        }
    }
    info!(count = role_registry.list().len(), "roles loaded");

    // F1: per-CLI model config (tier → concrete model). Shipped defaults if no
    // ~/.swarmx/models.json (≡ legacy behaviour). Hot-swapped on PUT /api/models.
    let model_config = models_config::load_or_default();
    info!(clis = model_config.clis.len(), "model config loaded");

    let shim_path = spawn::locate_shim().context("locate swarmx-shim")?;
    info!(shim = %shim_path.display(), "shim located");

    let mcp_bin = spawn::locate_mcp().context("locate swarmx-mcp")?;
    info!(mcp = %mcp_bin.display(), "mcp binary located");

    let workspaces_root = workspaces_root_default();
    std::fs::create_dir_all(&workspaces_root)?;
    info!(workspaces = %workspaces_root.display(), "workspaces root");

    let db_path = db_path_default();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Single-instance gate. swarmx state is split across sqlite, the
    // blackboard fs tree, and per-spawn workspaces, all under the same
    // data dir. A second server racing the first's `mark_orphan_*` calls
    // will silently flip the first's live agents to killed in the DB
    // (in-memory state survives, but the next restart inherits the
    // corruption). Take an exclusive flock on a lockfile next to the
    // DB; the lock auto-releases when this process exits (fd close).
    acquire_singleton_lock(&db_path)?;

    info!(db = %db_path.display(), "opening sqlite store");
    let store = Arc::new(Store::open(&db_path).await.context("open store")?);
    // Any agent / recording left "live" in the DB belongs to a previous
    // server process that died before kill / finalize. Settle them so the
    // UI doesn't keep reattaching to dead PTYs or showing "● live" forever.
    let orphan_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    match store.mark_orphan_agents_killed(orphan_at).await {
        Ok(n) if n > 0 => info!(orphans = n, "settled orphan live agents"),
        Ok(_) => {}
        Err(err) => tracing::warn!(?err, "mark_orphan_agents_killed failed"),
    }
    match store.mark_orphan_recordings_finalized(orphan_at).await {
        Ok(n) if n > 0 => info!(orphans = n, "settled orphan live recordings"),
        Ok(_) => {}
        Err(err) => tracing::warn!(?err, "mark_orphan_recordings_finalized failed"),
    }
    // Fusion recovery. A batch's progress guarantee (running→judging→done) lives
    // entirely in in-memory tokio tasks (the autochain + judge watchdog) — they
    // die with the process. A batch caught mid-flight by a restart (upgrade,
    // crash, sidecar relaunch) would otherwise spin forever: the orphan sweep
    // above just settled its contestant/judge agents, so nothing will ever land
    // it. Surface each stalled batch as 'needs_decision' — the UI's manual-pick
    // entry over the persisted contestant diffs. Honest and quota-safe: no silent
    // LLM re-run, no half-state.
    match store.list_active_fusion_batches().await {
        Ok(batches) => {
            for b in &batches {
                if let Err(err) = store
                    .set_fusion_status(b.id.clone(), "needs_decision".to_string())
                    .await
                {
                    tracing::warn!(?err, batch = %b.id, "fusion recovery: set needs_decision failed");
                }
            }
            if !batches.is_empty() {
                info!(recovered = batches.len(), "fusion recovery: stalled batches → needs_decision");
            }
        }
        Err(err) => tracing::warn!(?err, "list_active_fusion_batches failed"),
    }

    let blackboard_root = blackboard_root_default();
    std::fs::create_dir_all(&blackboard_root)?;
    info!(blackboard = %blackboard_root.display(), "blackboard root");
    let swarm = Swarm::new(store.clone(), blackboard_root.clone());
    let watcher = WatcherHandle::spawn(blackboard_root.clone(), swarm.clone())
        .context("spawn blackboard watcher")?;
    let watcher = Arc::new(watcher);

    // F6 completion: backfill op-log rows for any blackboard file left on disk
    // without one by a prior crash mid-write, so it's visible to discovery
    // again. Cheap + idempotent (one query + inserts only for missing paths).
    match swarm.reconcile_oplog_from_disk().await {
        Ok(n) if n > 0 => info!(
            reconciled = n,
            "backfilled blackboard op rows missing from a prior crash"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(?err, "blackboard op-log reconcile failed"),
    }

    // F5 retention: the three append-only tables (blackboard_ops, messages,
    // pty_recordings) otherwise grow forever. Trim rows past the retention
    // window — but only ones that are no longer load-bearing (superseded
    // blackboard history, consumed wakes + read messages, old finalized
    // recordings). SWARMX_RETENTION_DAYS=0 keeps everything. Runs once at
    // boot; never blocks startup on failure.
    if let Some(window_ms) = retention_window_ms() {
        let cutoff = orphan_at.saturating_sub(window_ms);
        match store.prune_expired(cutoff).await {
            Ok(stats) if !stats.is_empty() => info!(
                blackboard_ops = stats.blackboard_ops,
                messages = stats.messages,
                recordings = stats.recordings,
                cast_files = stats.recording_files_removed,
                agent_usage = stats.agent_usage,
                agent_activities = stats.agent_activities,
                "pruned expired rows past the retention window"
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(?err, "retention prune failed; continuing startup"),
        }
    }

    let recordings_root = recordings_root_default();
    std::fs::create_dir_all(&recordings_root)?;
    info!(recordings = %recordings_root.display(), "recordings root");

    // SWARMX_PORT is parsed here (not just at bind time) because it also
    // drives the URL we bake into spawned agents' wake-check hook + swarm MCP.
    // A parallel instance set with only SWARMX_PORT used to leave agents
    // pointing at the 7777 default (their messages then invisible to this
    // server). Derive SERVER_URL from PORT so PORT alone is sufficient;
    // an explicit SWARMX_SERVER_URL still wins.
    let port: u16 = std::env::var("SWARMX_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7777);
    let server_url =
        std::env::var("SWARMX_SERVER_URL").unwrap_or_else(|_| format!("http://127.0.0.1:{port}"));

    let wake_subs: wake::WakeSubs =
        std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let exit_keys: wake::ExitKeys =
        std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    let state = AppState {
        plugins: Arc::new(plugin_registry),
        spells: Arc::new(spell_registry),
        roles: Arc::new(role_registry),
        models: Arc::new(tokio::sync::RwLock::new(model_config)),
        registry: registry::Registry::new(),
        shim_path,
        mcp_bin,
        server_url,
        workspaces_root,
        store,
        swarm,
        blackboard_root,
        recordings_root,
        _watcher: watcher,
        wake_subs: wake_subs.clone(),
        exit_keys: exit_keys.clone(),
        // Small fixed budget: these are best-effort helper calls, not the
        // workhorse agents; SWARMX_MAX_ONESHOT_QUERIES overrides for testing.
        oneshot_limiter: Arc::new(tokio::sync::Semaphore::new(
            std::env::var("SWARMX_MAX_ONESHOT_QUERIES")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(4),
        )),
    };

    // M6b: launch the wake coordinator. Lives for the whole process; the
    // returned JoinHandle is intentionally dropped because the task exits
    // only when `state.swarm`'s broadcast closes (program shutdown).
    let _wake_handle = wake::WakeCoordinator::spawn(
        state.swarm.clone(),
        state.registry.clone(),
        wake_subs,
        exit_keys,
        state.store.clone(),
        state.server_url.clone(),
    );
    info!("wake coordinator started");

    cron::spawn_scheduler(state.clone());
    info!("cron scheduler started");

    // Liveness reaper: deterministic backstop that retires any agent whose
    // child process actually died without emitting/synthesizing a ShimExit, so
    // the UI never sits forever on a green dot for a process that is gone.
    reaper::spawn(state.clone());
    info!("liveness reaper started");

    // Periodic retention: the boot-time prune above runs once; this keeps the
    // append-only tables (messages / blackboard / recordings / agent_usage /
    // agent_activities) from growing unbounded across a long-lived session.
    // Skipped entirely when retention is disabled (SWARMX_RETENTION_DAYS=0).
    if retention_window_ms().is_some() {
        let prune_store = state.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(window) = retention_window_ms() else {
                    continue;
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let cutoff = now.saturating_sub(window);
                match prune_store.prune_expired(cutoff).await {
                    Ok(stats) if !stats.is_empty() => tracing::info!(
                        blackboard_ops = stats.blackboard_ops,
                        messages = stats.messages,
                        recordings = stats.recordings,
                        agent_usage = stats.agent_usage,
                        agent_activities = stats.agent_activities,
                        "periodic retention prune"
                    ),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(?err, "periodic retention prune failed"),
                }
            }
        });
        info!("retention prune loop started (every 6h)");
    }

    let mut app = routes::api::router();

    // Serve the built web bundle so `http://127.0.0.1:<port>` actually shows the
    // UI (apiBase.ts / require_local_origin already assume the server hosts it;
    // it was never wired → bare 404). The fallback serves static assets and, for
    // unknown client-routes, index.html (SPA history routing) — BUT a path under
    // /api or /ws must 404 cleanly instead of being swallowed into a 200
    // index.html (that would hand API callers HTML + a confusing JSON parse
    // error). No bundle on disk (a dev-only checkout) → stay API-only.
    if let Some(web_dir) = resolve_web_dir() {
        let serve = ServeDir::new(&web_dir).fallback(ServeFile::new(web_dir.join("index.html")));
        app = app.fallback(move |req: Request| {
            let serve = serve.clone();
            async move {
                let p = req.uri().path();
                if p.starts_with("/api/") || p.starts_with("/ws/") {
                    return StatusCode::NOT_FOUND.into_response();
                }
                serve.oneshot(req).await.into_response()
            }
        });
        info!(web = %web_dir.display(), "serving web bundle");
    } else {
        info!("no web bundle on disk; API-only (serve UI via `npm run dev` or Tauri)");
    }

    let app = app
        // Innermost: turn a handler panic into a 500 for that one request
        // instead of dropping the whole connection (which would also poison
        // other in-flight requests sharing it).
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        // CORS stays permissive so the Tauri webview's cross-origin fetch
        // (tauri://localhost → 127.0.0.1:7777) keeps working. The actual
        // security boundary is `require_local_origin` below, which rejects
        // any request whose Origin isn't a local host — including the
        // cross-site WebSocket upgrades that CORS does NOT cover.
        .layer(CorsLayer::permissive())
        // request-id plumbing. axum applies layers inside-out (the LAST .layer
        // runs FIRST on the request), so the effective order is: SetRequestId
        // (mint the uuid) → TraceLayer (read it into the span) → Propagate (copy
        // it onto the response). Every log line for one request then shares an
        // id, and x-request-id is echoed back to the client.
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
                let request_id = req
                    .headers()
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                tracing::info_span!(
                    "http",
                    %request_id,
                    method = %req.method(),
                    path = %req.uri().path(),
                )
            }),
        )
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid::default()))
        // Outermost layer (added last ⇒ runs first): drop cross-site
        // requests before they reach any handler.
        .layer(middleware::from_fn(require_local_origin))
        .with_state(state.clone());

    // Effective config dump — one place to see every resolved knob (pairs with
    // docs/configuration.md). Helps diagnose "why is it using THAT dir / port".
    info!(
        port,
        server_url = %state.server_url,
        db = %db_path_default().display(),
        workspaces = %state.workspaces_root.display(),
        blackboard = %state.blackboard_root.display(),
        recordings = %state.recordings_root.display(),
        retention_days = retention_window_ms().map(|ms| ms / 86_400_000).unwrap_or(0),
        "effective config"
    );

    // `port` parsed above (it also derives server_url). Bind loopback only.
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    info!(%addr, "swarmx-server listening (loopback only; cross-origin requests rejected)");
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Optional Magentic-One restart resilience: for every alive workspace whose
    // orchestrator was settled-as-killed by the orphan sweep above, re-spawn it.
    // This can burn an LLM turn and may revive a provider the operator is
    // intentionally avoiding, so it is opt-in; the chat view can still revive
    // on demand when the user actually returns.
    //
    // Done as tokio::spawn so it doesn't delay axum::serve. The
    // orchestrator role is loaded in spell_registry above; if the
    // user has deleted it or pinned to a different role, the spell
    // run fails loudly and the user can re-create the workspace.
    if std::env::var("SWARMX_AUTO_RESPAWN_ORCHESTRATORS").ok().as_deref() == Some("1") {
        tokio::spawn({
            let respawn_state = state.clone();
            async move {
                if let Err(err) = auto_respawn_orchestrators(&respawn_state).await {
                    tracing::warn!(?err, "auto-respawn orchestrators failed");
                }
            }
        });
    } else {
        info!("auto-respawn: disabled (set SWARMX_AUTO_RESPAWN_ORCHESTRATORS=1 to enable)");
    }

    // LiteLLM fallback catalog: load ~/.swarmx cache, then one-shot GitHub refresh
    // (cc-switch-style). Failures keep embedded/disk; opt out with
    // SWARMX_DISABLE_LITELLM_REFRESH=1.
    routes::usage::spawn_litellm_pricing_refresh();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state.registry.clone()))
        .await?;
    Ok(())
}

/// 遍历所有未删 workspace,对没活的 orchestrator 的 workspace 拉一个新的
/// (走 init spell 路径)。直接调 `routes::rest::run_spell` 不绕 HTTP,因为
/// state 已经持有所有必要句柄(spell registry / role registry / store /
/// registry / swarm / wake / exit_keys)。
///
/// 调用时机:server 启动 + orphan settle 之后,axum 开始 serve 之后(用
/// `tokio::spawn` 异步并发,不阻塞 listen)。
async fn auto_respawn_orchestrators(state: &AppState) -> Result<()> {
    use axum::extract::{Json as AxJson, State};
    use swarmx_protocol::rest::RunSpellRequest;

    let workspaces = state
        .store
        .list_workspaces(false)
        .await
        .context("list_workspaces")?;
    if workspaces.is_empty() {
        info!("auto-respawn: no workspaces, skipping");
        return Ok(());
    }

    let agents = state.store.list_agents().await.context("list_agents")?;

    let mut respawned = 0usize;
    let mut skipped = 0usize;
    for ws in &workspaces {
        let has_live_orchestrator = agents.iter().any(|a| {
            a.workspace_id.as_deref() == Some(ws.id.as_str())
                && a.role == "orchestrator"
                && a.killed_at.is_none()
        });
        if has_live_orchestrator {
            skipped += 1;
            continue;
        }

        // Don't respawn into a directory that no longer exists — the init
        // spell's "create shared workspace" step would just fail and log
        // noise. The workspace row stays (user can delete it from the UI).
        if !std::path::Path::new(&ws.cwd).is_dir() {
            skipped += 1;
            info!(workspace_id = %ws.id, "auto-respawn: cwd missing, skipping");
            continue;
        }

        // Don't respawn a FINISHED workspace on boot. Re-spawning runs the
        // init spell, which injects a bootstrap prompt and burns a full LLM
        // turn — for a completed task that just re-concludes "nothing to do",
        // wasting subscription budget on every server restart. The progress
        // ledger carries an `all_done` marker once the orchestrator wrapped
        // up; if we see it, leave the workspace member-less. The UI's
        // 0-member chat state offers a "wake orchestrator" button that runs
        // init on demand when the user actually returns to the project.
        //
        // Auto-respawn revives the MAIN direction, whose ledger now lives at
        // `{ws}/main/progress.ledger.md` (per-direction prefix). Also check the
        // legacy `{ws}/progress.ledger.md` so workspaces completed before the
        // thread-prefix migration still skip (one-time; regenerates on wake).
        let main_key = format!("{}/main/progress.ledger.md", ws.id);
        let legacy_key = format!("{}/progress.ledger.md", ws.id);
        let mut all_done = false;
        for key in [&main_key, &legacy_key] {
            if let Ok(Some(content)) = state.swarm.read_blackboard(key).await {
                if content.contains("all_done") {
                    all_done = true;
                    break;
                }
            }
        }
        if all_done {
            skipped += 1;
            info!(workspace_id = %ws.id, "auto-respawn: task all_done, skipping (revive on demand)");
            continue;
        }
        let req = RunSpellRequest {
            name: "init".into(),
            task: String::new(),
            workspace_dir: Some(ws.cwd.clone()),
            workspace_id: Some(ws.id.clone()),
            caller_agent_id: None,
            // Auto-respawn revives the orchestrator on the workspace's main
            // thread; run_spell resolves it (oldest thread) when this is None.
            thread_id: None,
            // Preserve the captain engine across restart (else an opencode
            // captain would silently come back as the role-default claude).
            captain_cli: routes::rest::last_orchestrator_cli(&state, &ws.id).await,
        };
        match routes::rest::run_spell(State(state.clone()), AxJson(req)).await {
            Ok(resp) => {
                respawned += 1;
                info!(
                    workspace_id = %ws.id,
                    workspace_name = %ws.name,
                    spawned = resp.0.agents.len(),
                    "auto-respawn: orchestrator re-spawned"
                );
            }
            Err((status, body)) => {
                tracing::warn!(
                    workspace_id = %ws.id,
                    %status,
                    body = %body.0,
                    "auto-respawn: init spell failed"
                );
            }
        }
    }
    info!(
        respawned,
        skipped,
        total = workspaces.len(),
        "auto-respawn: complete"
    );
    Ok(())
}

fn workspaces_root_default() -> PathBuf {
    if let Ok(p) = std::env::var("SWARMX_WORKSPACES_DIR") {
        return PathBuf::from(p);
    }
    if let Some(home) = crate::runtime_path::swarmx_home() {
        return home.join(".swarmx").join("workspaces");
    }
    PathBuf::from(".swarmx/workspaces")
}

/// `swarmx-server doctor` — preflight self-check. Prints a red/green list of
/// what a first launch needs (shim/mcp binaries, the CLIs on PATH, a free port,
/// a writable data dir) and exits non-zero if any hard requirement is missing,
/// so a new user gets an actionable diagnosis instead of a confusing mid-boot
/// failure (e.g. the "missing shim → startup fails" footgun).
async fn run_doctor() -> Result<()> {
    println!("swarmx doctor — preflight check\n");
    let mut all_ok = true;
    macro_rules! check {
        ($label:expr, $pass:expr, $detail:expr) => {{
            let pass: bool = $pass;
            println!("{} {}: {}", if pass { "✓" } else { "✗" }, $label, $detail);
            all_ok &= pass;
        }};
    }

    match spawn::locate_shim() {
        Ok(p) => check!("swarmx-shim", true, p.display()),
        Err(e) => check!(
            "swarmx-shim",
            false,
            format!("{e} — run `cargo build --workspace`")
        ),
    }
    match spawn::locate_mcp() {
        Ok(p) => check!("swarmx-mcp", true, p.display()),
        Err(e) => check!("swarmx-mcp", false, e.to_string()),
    }
    // CLIs are a soft warning: you can run swarmx with only one of them.
    for cli in ["claude", "codex"] {
        match on_path(cli) {
            Some(p) => println!("✓ {cli}: {}", p.display()),
            None => println!("⚠ {cli}: not on PATH (install it before spawning {cli} agents)"),
        }
    }
    let port: u16 = std::env::var("SWARMX_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7777);
    match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(_) => check!("port", true, format!("{port} is free")),
        Err(e) => check!(
            "port",
            false,
            format!("{port} — {e} (another swarmx running, or a stale orphan holding it?)")
        ),
    }
    let db = db_path_default();
    let dir_ok = db
        .parent()
        .map(|p| p.exists() || std::fs::create_dir_all(p).is_ok())
        .unwrap_or(false);
    check!(
        "data dir",
        dir_ok,
        db.parent().map(|p| p.display().to_string()).unwrap_or_default()
    );

    println!();
    if all_ok {
        println!("All checks passed.");
        Ok(())
    } else {
        println!("Some checks failed — fix the ✗ items above before launching.");
        std::process::exit(1);
    }
}

/// First match for `bin` on `$PATH`, if any — via the shared PATHEXT-aware
/// resolver so Windows `.cmd`/`.bat` shims (npm-installed CLIs) are found and the
/// doctor check agrees with what actually spawns.
fn on_path(bin: &str) -> Option<PathBuf> {
    crate::runtime_path::resolve_executable(bin)
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM. Before letting `axum::serve` drain
/// and exit, it **reaps every live agent's process group** so the spawned CLIs
/// don't orphan.
///
/// Why this is explicit and not left to Drop: a long-lived WS/SSE subscriber
/// (e.g. an attached terminal, or reasonix's `/events` follower) keeps axum's
/// graceful drain from ever completing, so `serve()` never returns and the Arc
/// `Drop`s that *would* tear down the PTYs never run — the server lingers and
/// the agents (shim → CLI → its descendants, e.g. reasonix's
/// node→serve→swarmx-mcp tree) survive reparented to init, still holding
/// ports and burning tokens. Killing the agents here closes their PTYs, which
/// also lets those stuck connections end so the drain can finish.
async fn shutdown_signal(registry: registry::Registry) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received — reaping agents + draining connections");

    // PtyBridge::kill() signals the whole process group (SIGTERM → ~1s grace →
    // SIGKILL) and blocks, so run the reaping off the async worker.
    let agents = registry.list();
    if !agents.is_empty() {
        let count = agents.len();
        let _ = tokio::task::spawn_blocking(move || {
            for (id, slot) in agents {
                slot.lock().kill();
                tracing::debug!(agent = %id, "reaped agent process group on shutdown");
            }
        })
        .await;
        tracing::info!(count, "reaped live agents on shutdown");
    }
}

/// Retention window in ms from `SWARMX_RETENTION_DAYS` (default 30 days).
/// Returns `None` when the var is set to 0 or negative — "keep everything,
/// never prune". A non-numeric value falls back to the default.
fn retention_window_ms() -> Option<i64> {
    let days: i64 = std::env::var("SWARMX_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(30);
    (days > 0).then(|| days * 24 * 60 * 60 * 1000)
}

fn log_dir_default() -> PathBuf {
    if let Some(home) = crate::runtime_path::swarmx_home() {
        return home.join(".swarmx").join("logs");
    }
    PathBuf::from(".swarmx/logs")
}

fn db_path_default() -> PathBuf {
    if let Ok(p) = std::env::var("SWARMX_DB_PATH") {
        return PathBuf::from(p);
    }
    if let Some(home) = crate::runtime_path::swarmx_home() {
        return home.join(".swarmx").join("swarmx.db");
    }
    PathBuf::from(".swarmx/swarmx.db")
}

fn blackboard_root_default() -> PathBuf {
    if let Ok(p) = std::env::var("SWARMX_BLACKBOARD_DIR") {
        return PathBuf::from(p);
    }
    if let Some(home) = crate::runtime_path::swarmx_home() {
        return home.join(".swarmx").join("blackboard");
    }
    PathBuf::from(".swarmx/blackboard")
}

fn recordings_root_default() -> PathBuf {
    if let Ok(p) = std::env::var("SWARMX_RECORDINGS_DIR") {
        return PathBuf::from(p);
    }
    if let Some(home) = crate::runtime_path::swarmx_home() {
        return home.join(".swarmx").join("recordings");
    }
    PathBuf::from(".swarmx/recordings")
}

/// Try to acquire an exclusive non-blocking flock on `<db_dir>/server.lock`.
/// The fd is intentionally leaked so the lock outlives this function — it is
/// released by the kernel when the process exits, which is exactly the
/// "lock until shutdown" semantic we want. We also stamp the holding PID
/// into the lockfile body so users running `cat ~/.swarmx/server.lock`
/// can see who owns it.
///
/// On contention we exit(2) with a multi-line error pointing the user at
/// the holding pid and a remediation command. We do this *before* opening
/// the sqlite store so a refused boot leaves zero footprint.
fn acquire_singleton_lock(db_path: &std::path::Path) -> Result<()> {
    use fs2::FileExt;
    use std::io::{Read, Write};

    let lock_path = db_path
        .parent()
        .map(|p| p.join("server.lock"))
        .unwrap_or_else(|| PathBuf::from(".swarmx-server.lock"));

    let mut lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open lockfile {}", lock_path.display()))?;

    if let Err(e) = lock_file.try_lock_exclusive() {
        let mut holder_pid = String::new();
        let _ = lock_file.read_to_string(&mut holder_pid);
        let holder_pid = holder_pid.trim();
        eprintln!();
        eprintln!("✗ swarmx-server: another instance is already running.");
        eprintln!();
        eprintln!("  Lock file : {}", lock_path.display());
        if !holder_pid.is_empty() {
            eprintln!("  Holder PID: {holder_pid}");
        }
        eprintln!("  Reason    : {e}");
        eprintln!();
        eprintln!("  Two instances share ~/.swarmx (sqlite, blackboard, workspaces)");
        eprintln!("  and would corrupt each other's state. Stop the existing process");
        eprintln!("  first, e.g.:");
        if !holder_pid.is_empty() {
            eprintln!("    kill {holder_pid}");
        } else {
            eprintln!("    pkill -f swarmx-server");
        }
        eprintln!();
        std::process::exit(2);
    }

    // Lock acquired — rewrite the body with our PID for the next contender.
    let _ = lock_file.set_len(0);
    let _ = lock_file.write_all(format!("{}\n", std::process::id()).as_bytes());

    // Move ownership into a Box::leak so the fd stays open for the rest
    // of the process. Dropping the File would release the lock prematurely
    // if the function returned without this.
    Box::leak(Box::new(lock_file));
    info!(lock = %lock_path.display(), "acquired singleton lock");
    Ok(())
}

/// Locate the built web bundle (`dist/` with an `index.html`) to serve at the
/// server root. Resolution order: `SWARMX_WEB_DIR` (explicit, e.g. the Tauri
/// bundle's resource dir) → `<cwd>/web/dist` or `<cwd>/dist` (running from the
/// repo) → next to / two levels up from the executable (`target/release` →
/// repo `web/dist`). Returns `None` when nothing is found, so a bare dev
/// checkout stays API-only instead of crashing.
fn resolve_web_dir() -> Option<PathBuf> {
    let has_index = |p: &PathBuf| p.join("index.html").is_file();
    if let Ok(p) = std::env::var("SWARMX_WEB_DIR") {
        let pb = PathBuf::from(p);
        if has_index(&pb) {
            return Some(pb);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("web/dist"));
        candidates.push(cwd.join("dist"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("web/dist"));
            candidates.push(dir.join("../../web/dist")); // target/release → repo/web/dist
        }
    }
    candidates.into_iter().find(has_index)
}

/// Reject cross-site requests. swarmx binds loopback with no token auth,
/// but a browser will happily open a cross-origin WebSocket to
/// `ws://127.0.0.1:7777` (WS bypasses CORS and sends no preflight), so any
/// web page the user happens to visit could otherwise spawn agents, inject
/// keystrokes into a live PTY, or read the blackboard.
///
/// Gate on the `Origin` header:
///   * no `Origin` → native client (the MCP subprocess via reqwest, curl,
///     the Tauri webview's own asset-scheme requests) → **allow**.
///   * `Origin` present → allow only if its host is a local host (loopback or
///     `*.localhost`): covers vite dev (`http://localhost:5173`), the bundle
///     served on `:7777`, and the Tauri webview (`tauri://localhost` /
///     `tauri.localhost`). Anything else (e.g. `http://evil.com`) → **403**.
async fn require_local_origin(req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let allowed = origin.to_str().ok().map(origin_is_local).unwrap_or(false);
        if !allowed {
            tracing::warn!(?origin, "rejected cross-origin request");
            return (
                StatusCode::FORBIDDEN,
                "cross-origin request rejected (swarmx is loopback-only)",
            )
                .into_response();
        }
    } else {
        // No Origin = a native client (MCP reqwest, curl, the Tauri webview's
        // asset-scheme requests) — all of which hit 127.0.0.1 directly. So
        // *additionally* require the Host to be loopback: a DNS-rebind request
        // (Host: attacker.com resolving to 127.0.0.1, which browsers send with
        // NO Origin on top-level / `<img>` / form-GET navigations) is then
        // rejected, while genuine native clients still pass. A missing Host
        // (rare; HTTP/2 carries the authority in the URI) is allowed.
        let host_ok = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .or_else(|| req.uri().host())
            .map(host_is_loopback)
            .unwrap_or(true);
        if !host_ok {
            tracing::warn!("rejected no-Origin request with non-loopback Host");
            return (
                StatusCode::FORBIDDEN,
                "request rejected (swarmx is loopback-only)",
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// True if a `Host` header value (`127.0.0.1:7777`, `localhost:5173`,
/// `[::1]:7777`) points at a loopback host. Used to reject no-Origin DNS-rebind
/// requests, whose Host carries the attacker's own domain.
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim();
    let h = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: `[::1]` / `[::1]:port`.
        rest.split(']').next().unwrap_or("")
    } else {
        // `host` or `host:port` — strip a trailing `:port`.
        host.rsplit_once(':').map(|(a, _)| a).unwrap_or(host)
    };
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    h.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// True if an `Origin` header value (e.g. `http://localhost:5173`,
/// `tauri://localhost`, `https://tauri.localhost`) points at a local host.
/// A literal `null` origin (sandboxed iframe / `file://`) has no `://` host
/// component → not local → rejected.
fn origin_is_local(origin: &str) -> bool {
    let host = match origin.split_once("://") {
        Some((_scheme, rest)) => rest.split(['/', ':']).next().unwrap_or(""),
        None => return false,
    };
    host == "127.0.0.1"
        || host == "localhost"
        || host == "::1"
        || host == "[::1]"
        || host.ends_with(".localhost")
}

#[cfg(test)]
mod origin_tests {
    use super::origin_is_local;

    #[test]
    fn allows_local_origins() {
        for o in [
            "http://localhost:5173", // vite dev
            "http://127.0.0.1:7777", // bundle served by the server
            "http://localhost:7777",
            "http://127.0.0.1:4173",   // vite preview / sidecar test recipe
            "tauri://localhost",       // macOS asset scheme
            "https://tauri.localhost", // Windows/Linux asset scheme
            "http://tauri.localhost",
        ] {
            assert!(origin_is_local(o), "should allow {o}");
        }
    }

    #[test]
    fn rejects_remote_and_null() {
        for o in [
            "http://evil.com",
            "https://attacker.example:443",
            "http://localhost.evil.com", // suffix attack — host is .evil.com
            "http://notlocalhost",
            "null", // sandboxed iframe / file://
            "",
        ] {
            assert!(!origin_is_local(o), "should reject {o}");
        }
    }

    #[test]
    fn host_loopback_gate() {
        use super::host_is_loopback;
        for h in [
            "127.0.0.1:7777",
            "127.0.0.1",
            "localhost:5173",
            "localhost",
            "[::1]:7777",
            "foo.localhost:7777",
        ] {
            assert!(host_is_loopback(h), "should allow {h}");
        }
        for h in [
            "evil.com",
            "attacker.example:443",
            "169.254.1.1",      // link-local, not loopback
            "10.0.0.5:7777",    // private LAN
            "localhost.evil.com", // suffix attack
        ] {
            assert!(!host_is_loopback(h), "should reject {h}");
        }
    }
}
