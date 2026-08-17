//! `GET/PUT /api/workspaces/:id/budget` — the workspace budget brake's
//! operator surface (storage: migration 0030; enforcement: crate::budget).
//!
//! The budget is an OPTIONAL all-time estimated-spend cap (USD), matching the
//! all-time nature of `/api/usage` totals. NULL/0 = unlimited. Every number
//! here is an ESTIMATE from transcript scraping priced by the editable table
//! in `usage.rs` — never the subscription invoice; the response and all UI
//! copy say so.
//!
//! PUT semantics:
//!   - set a cap below/at the current estimate → the brake TRIPS immediately
//!     (live agents paused, marker persisted, BudgetChanged broadcast);
//!   - raise the cap above the current estimate (or clear it) while tripped →
//!     the brake LIFTS and the agents it paused get resumed.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SetBudgetRequest {
    /// USD cap. `null` (or a non-positive number) clears to unlimited.
    pub budget_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BudgetStatus {
    workspace_id: String,
    /// The cap in force; `null` = unlimited.
    budget_usd: Option<f64>,
    /// Current all-time ESTIMATED spend — the same number `/api/usage` totals.
    current_cost_usd: f64,
    /// false when some models were unpriced: the real spend is >= the estimate.
    priced: bool,
    /// Brake state: true = tripped (agents paused, spawns/turns refused).
    exceeded: bool,
    /// Trip-time estimate + timestamp (None while not tripped).
    trip_cost_usd: Option<f64>,
    trip_at: Option<i64>,
}

async fn status_for(state: &AppState, workspace_id: &str) -> Result<BudgetStatus, (StatusCode, Json<serde_json::Value>)> {
    let ws = state
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
        })?;
    let (cost, priced) = crate::routes::usage::workspace_cost_estimate(&state.store, workspace_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    Ok(BudgetStatus {
        workspace_id: ws.id,
        budget_usd: ws.budget_usd.filter(|b| *b > 0.0),
        current_cost_usd: cost,
        priced,
        exceeded: ws.budget_exceeded_at.is_some(),
        trip_cost_usd: ws.budget_exceeded_cost_usd,
        trip_at: ws.budget_exceeded_at,
    })
}

/// `GET /api/workspaces/:id/budget` — current cap + live estimate + brake state.
pub async fn get_budget(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<BudgetStatus>, (StatusCode, Json<serde_json::Value>)> {
    status_for(&state, &workspace_id).await.map(Json)
}

/// `PUT /api/workspaces/:id/budget` — set or clear the cap, then reconcile
/// the brake against the live estimate (trip / lift as needed).
pub async fn put_budget(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<SetBudgetRequest>,
) -> Result<Json<BudgetStatus>, (StatusCode, Json<serde_json::Value>)> {
    // NULL/0 = unlimited. Reject nonsense (NaN/inf/negative) honestly instead
    // of silently storing a cap that never trips.
    let budget = match req.budget_usd {
        Some(b) if !b.is_finite() || b < 0.0 => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "budget_usd must be a finite number >= 0 (0/null clears the cap)"})),
            ));
        }
        Some(b) if b > 0.0 => Some(b),
        _ => None,
    };
    let ws = state
        .store
        .set_workspace_budget(workspace_id.clone(), budget)
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

    let (cost, _priced) = crate::routes::usage::workspace_cost_estimate(&state.store, &workspace_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;

    let tripped = ws.budget_exceeded_at.is_some();
    match budget {
        // Cap at/below the current estimate → brake ON. Idempotent inside
        // (repeat PUTs while tripped don't re-pause or re-notify).
        Some(b) if cost >= b => {
            crate::budget::trip_workspace(
                &state.swarm,
                &state.registry,
                &state.store,
                &workspace_id,
                budget,
                cost,
            )
            .await;
        }
        // Cleared, or raised above the estimate → lift a tripped brake.
        _ if tripped => {
            crate::budget::lift_workspace(
                &state.swarm,
                &state.registry,
                &state.store,
                &state.server_url,
                &workspace_id,
                budget,
                cost,
            )
            .await;
        }
        _ => {}
    }
    status_for(&state, &workspace_id).await.map(Json)
}
