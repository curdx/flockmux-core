//! Dispatch for the four JSON-RPC methods we expose:
//!   - `initialize`                  — handshake; returns protocol version + caps
//!   - `notifications/initialized`   — no response (notification)
//!   - `tools/list`                  — array of tool descriptors
//!   - `tools/call`                  — invoke one of the eleven swarm_* tools
//!
//! Anything else returns METHOD_NOT_FOUND. We never panic on bad params —
//! claude treats a closed stdio as a server crash and stops trying to use it.

use crate::protocol::{JsonRpcResponse, INVALID_PARAMS, METHOD_NOT_FOUND};
use crate::tools::{call_tool, tool_descriptors, ToolContext};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

/// Protocol version we advertise on `initialize`. The string is checked by
/// claude / codex against their supported set; if they upgrade past this
/// version, the symptom is the server failing the handshake and bump
/// strings here.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Fired (once per process) the first time the CLI fetches our tool list:
/// per the MCP lifecycle that's the moment the `swarm_*` tools become visible
/// to the model, so it's the canonical "this agent is ready to coordinate"
/// signal. We POST it to swarmx-server so the deferred bootstrap can inject
/// the agent's prompt immediately instead of waiting out a fixed timeout
/// (readiness-probe pattern). Fire-and-forget: a failed/late ping just means
/// the server falls back to its bootstrap timeout — never blocks the tool
/// response.
static MCP_READY_PINGED: AtomicBool = AtomicBool::new(false);

fn fire_mcp_ready_once(ctx: &ToolContext) {
    if MCP_READY_PINGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let http = ctx.http.clone();
    let url = format!(
        "{}/api/agent/{}/mcp-ready",
        ctx.server_url.trim_end_matches('/'),
        ctx.agent_id
    );
    tokio::spawn(async move {
        let _ = http.post(&url).send().await;
    });
}

pub async fn dispatch(
    ctx: &ToolContext,
    id: Value,
    method: &str,
    params: Option<Value>,
) -> JsonRpcResponse {
    match method {
        "initialize" => JsonRpcResponse::ok(id, initialize()),
        "tools/list" => {
            // The CLI just pulled the tool surface ⇒ swarm_* tools are now in
            // the model's toolset. Signal readiness so the bootstrap fires.
            fire_mcp_ready_once(ctx);
            JsonRpcResponse::ok(id, tools_list())
        }
        "tools/call" => match tools_call(ctx, params).await {
            Ok(result) => JsonRpcResponse::ok(id, result),
            Err(msg) => JsonRpcResponse::err(id, INVALID_PARAMS, msg),
        },
        other => JsonRpcResponse::err(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        ),
    }
}

fn initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": {
            "name": "swarmx-swarm",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools": { "listChanged": false }
        }
    })
}

fn tools_list() -> Value {
    json!({ "tools": tool_descriptors() })
}

async fn tools_call(ctx: &ToolContext, params: Option<Value>) -> Result<Value, String> {
    let params = params.ok_or_else(|| "missing params".to_string())?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'name' in tools/call params".to_string())?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(call_tool(ctx, name, &args).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dummy_ctx() -> ToolContext {
        ToolContext::new(
            "claude-test".into(),
            "http://127.0.0.1:1".into(), // never actually hit in these tests
        )
        .unwrap()
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version() {
        let ctx = dummy_ctx();
        let resp = dispatch(&ctx, json!(1), "initialize", None).await;
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], "swarmx-swarm");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_returns_full_tool_surface() {
        let ctx = dummy_ctx();
        let resp = dispatch(&ctx, json!(2), "tools/list", None).await;
        let result = resp.result.unwrap();
        let arr = result["tools"].as_array().unwrap();
        // 7 swarm primitives (3 messages + 4 blackboard/agents) +
        // swarm_spawn_worker (delegation entry) + swarm_list_roles (P0 role
        // catalog) + swarm_name_thread (multi-direction naming/isolation) +
        // swarm_fusion_consult (research-committee consult) = 11.
        // Keep in sync with tools::tool_descriptors() (asserted at tools.rs).
        assert_eq!(arr.len(), 11);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let ctx = dummy_ctx();
        let resp = dispatch(&ctx, json!(3), "ping", None).await;
        let err = resp.error.unwrap();
        assert_eq!(err.code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_call_without_params_returns_invalid_params() {
        let ctx = dummy_ctx();
        let resp = dispatch(&ctx, json!(4), "tools/call", None).await;
        let err = resp.error.unwrap();
        assert_eq!(err.code, INVALID_PARAMS);
    }
}
