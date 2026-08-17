//! Central HTTP/WS route registry.
//!
//! Keep URL paths and method bindings here so process startup stays focused on
//! state construction, middleware, and serving.

use axum::{
    routing::{delete, get, patch, post, put},
    Router,
};

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/plugins", get(super::rest::list_plugins))
        .route(
            "/api/plugins/probe",
            post(super::rest::probe_engines).get(super::rest::probe_status),
        )
        .route(
            "/api/plugins/:id/install",
            post(super::rest::install_plugin),
        )
        .route("/api/usage", get(super::usage::usage_summary))
        .route(
            "/api/usage/pricing",
            get(super::usage::usage_pricing_get)
                .put(super::usage::usage_pricing_put)
                .delete(super::usage::usage_pricing_reset),
        )
        .route("/api/tasks", get(super::tasks::list_tasks))
        .route("/api/tasks/:id/status", post(super::tasks::set_task_status))
        .route("/api/files/list", get(super::files::list_dir))
        .route("/api/files/read", get(super::files::read_file))
        .route("/ws/terminal", get(super::terminal_ws::terminal_ws))
        .route(
            "/api/cron",
            get(super::cron::list_cron).post(super::cron::create_cron),
        )
        .route("/api/cron/preview", get(super::cron::preview_cron))
        .route(
            "/api/cron/:id",
            delete(super::cron::delete_cron)
                .patch(super::cron::toggle_cron)
                .put(super::cron::update_cron),
        )
        .route("/api/cron/:id/run", post(super::cron::run_cron))
        .route(
            "/api/models",
            get(super::models_admin::get_models).put(super::models_admin::put_models),
        )
        .route("/api/mcp/env", get(super::mcp_admin::mcp_env))
        .route("/api/mcp/status", get(super::mcp_admin::mcp_status))
        .route(
            "/api/comate",
            get(super::comate::get_license).put(super::comate::put_license),
        )
        .route("/api/zulu/models", get(super::comate::zulu_models))
        .route("/api/mcp/install", post(super::mcp_admin::mcp_install))
        .route("/api/mcp/uninstall", post(super::mcp_admin::mcp_uninstall))
        .route(
            "/api/agent",
            get(super::rest::list_agents).post(super::rest::spawn),
        )
        .route("/api/worker", post(super::rest::spawn_worker))
        .route("/api/roles", get(super::rest::list_roles))
        .route("/api/agent/:id", delete(super::rest::kill))
        .route("/api/agent/:id/wake", post(super::rest::wake_agent))
        .route(
            "/api/agent/:id/activity",
            get(super::rest::agent_activity).post(super::rest::post_agent_activity),
        )
        .route("/api/agent/:id/interrupt", post(super::rest::interrupt))
        .route("/api/agent/:id/resume", post(super::rest::resume))
        .route("/api/agent/:id/mcp-ready", post(super::rest::mcp_ready))
        .route("/api/agent/interrupt-all", post(super::rest::interrupt_all))
        .route(
            "/api/message",
            get(super::swarm::list_messages).post(super::swarm::send_message),
        )
        .route("/api/debug/log", post(super::swarm::web_debug_log))
        .route("/api/message/read", post(super::swarm::mark_messages_read))
        .route(
            "/api/message/consume_wakes",
            post(super::swarm::consume_wakes),
        )
        .route("/api/blackboard", get(super::swarm::list_blackboard_paths))
        .route(
            "/api/blackboard/*path",
            get(super::swarm::read_blackboard)
                .put(super::swarm::write_blackboard)
                .delete(super::swarm::delete_blackboard),
        )
        .route(
            "/api/blackboard-history/*path",
            get(super::swarm::blackboard_history),
        )
        .route("/api/recording", get(super::recording::list_recordings))
        .route("/api/recording/:id", get(super::recording::get_recording))
        .route(
            "/api/workspaces",
            get(super::workspaces::list_workspaces_handler)
                .post(super::workspaces::create_workspace_handler),
        )
        .route(
            "/api/workspaces/:id",
            delete(super::workspaces::delete_workspace_handler),
        )
        .route(
            "/api/workspaces/:id/roots",
            post(super::workspaces::add_workspace_root_handler)
                .delete(super::workspaces::delete_workspace_root_handler),
        )
        .route(
            "/api/workspaces/:id/root-suggestions",
            get(super::workspaces::suggest_workspace_roots_handler),
        )
        .route(
            "/api/workspaces/:id/branches",
            get(super::workspaces::list_branches_handler),
        )
        .route(
            "/api/workspaces/:id/budget",
            get(super::budget::get_budget).put(super::budget::put_budget),
        )
        .route(
            "/api/workspaces/:id/threads",
            get(super::workspaces::list_threads_handler)
                .post(super::workspaces::create_thread_handler),
        )
        .route(
            "/api/workspaces/:id/threads/:tid",
            patch(super::workspaces::update_thread_handler)
                .delete(super::workspaces::delete_thread_handler),
        )
        .route(
            "/api/workspaces/:id/threads/:tid/model",
            put(super::workspaces::set_thread_model_handler),
        )
        .route(
            "/api/workspaces/:id/threads/:tid/diff",
            get(super::workspaces::thread_diff_handler),
        )
        .route(
            "/api/workspaces/:id/threads/:tid/merge",
            post(super::workspaces::merge_thread_handler),
        )
        .route(
            "/api/workspaces/:id/fusion",
            get(super::workspaces::list_fusion_handler)
                .post(super::workspaces::create_fusion_handler),
        )
        .route(
            "/api/workspaces/:id/fusion/:bid/judge",
            post(super::workspaces::judge_fusion_handler),
        )
        .route(
            "/api/workspaces/:id/fusion/:bid/decide",
            post(super::workspaces::decide_fusion_handler),
        )
        .route(
            "/api/workspaces/:id/fusion-consult",
            post(super::workspaces::fusion_consult_handler),
        )
        .route("/api/spells", get(super::rest::list_spells))
        .route("/api/spell/run", post(super::rest::run_spell))
        .route(
            "/api/goals",
            get(super::goals::list_goals).post(super::goals::create_goal),
        )
        .route(
            "/api/goals/:id/status",
            patch(super::goals::update_goal_status),
        )
        .route(
            "/api/goals/:id/evidence",
            get(super::goals::list_goal_evidence).post(super::goals::add_goal_evidence),
        )
        .route("/api/prompt/optimize", post(super::rest::optimize_prompt))
        .route(
            "/api/blackboard/compact",
            post(super::rest::compact_blackboard),
        )
        .route("/api/file", get(super::rest::serve_file))
        .route(
            "/api/attachment",
            post(super::rest::upload_attachment)
                .layer(axum::extract::DefaultBodyLimit::max(26_214_400)),
        )
        .route("/ws/swarm", get(super::ws_swarm::ws_swarm))
        .route("/ws/pty/:agent_id", get(super::pty_ws::pty_ws))
}
