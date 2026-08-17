//! `GET /api/usage` — token/cost observability.
//!
//! swarmx can't ask claude/codex for spend (we drive them over a PTY, not an
//! API), so the transcript tailer scrapes per-turn token counts from each
//! worker's session JSONL into `agent_usage` (migration 0016). This endpoint
//! aggregates that table and applies a pricing table to derive cost.
//!
//! Pricing lives HERE (not in the DB) so re-pricing never needs a migration.
//! The rates below are approximate published list prices (USD / 1M tokens,
//! 2026) — a model we don't recognise contributes tokens but `cost_usd = 0`
//! and flips `priced = false` so the UI can show "tokens only" honestly.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

use crate::AppState;

/// USD per 1,000,000 tokens. (input, output, cache_read, cache_write).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Rate {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

/// Embedded LiteLLM pricing snapshot (USD per 1M tokens), shipped via
/// `scripts/update-litellm-pricing.mjs`. Runtime may overlay a fresher table
/// fetched from upstream (cc-switch-style: at most once per process; failures
/// keep the embedded/disk copy). Primary editable rules still win on match.
const LITELLM_PRICING_JSON: &str = include_str!("../../resources/litellm_pricing.json");

/// Upstream catalog (USD **per token**). Slimmed to our per-1M shape on refresh.
const LITELLM_UPSTREAM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

#[derive(Clone, Copy, Debug, Deserialize)]
struct LiteLlmEntry {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    #[serde(default)]
    context_window: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LitellmOrigin {
    Embedded,
    Disk,
    Refreshed,
}

impl LitellmOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Disk => "disk",
            Self::Refreshed => "refreshed",
        }
    }
}

struct LitellmState {
    table: std::collections::HashMap<String, LiteLlmEntry>,
    origin: LitellmOrigin,
}

fn litellm_state() -> &'static parking_lot::RwLock<LitellmState> {
    static STATE: std::sync::OnceLock<parking_lot::RwLock<LitellmState>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| {
        let table = parse_slim_litellm_json(LITELLM_PRICING_JSON).unwrap_or_else(|err| {
            tracing::error!(?err, "embedded litellm_pricing.json failed to parse; fallback pricing disabled");
            std::collections::HashMap::new()
        });
        parking_lot::RwLock::new(LitellmState {
            table,
            origin: LitellmOrigin::Embedded,
        })
    })
}

fn parse_slim_litellm_json(
    raw: &str,
) -> Result<std::collections::HashMap<String, LiteLlmEntry>, serde_json::Error> {
    serde_json::from_str(raw)
}

fn litellm_cache_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".swarmx").join("litellm_pricing.json");
    }
    PathBuf::from(".swarmx/litellm_pricing.json")
}

fn litellm_refresh_disabled() -> bool {
    crate::billing::env_truthy("SWARMX_DISABLE_LITELLM_REFRESH")
}

/// Load disk cache (if any) over the embedded table, then spawn a one-shot
/// upstream refresh. Safe to call once at process start.
pub fn spawn_litellm_pricing_refresh() {
    // Overlay last successful refresh so cold start is fresh even offline.
    let path = litellm_cache_path();
    if let Ok(txt) = std::fs::read_to_string(&path) {
        match parse_slim_litellm_json(&txt) {
            Ok(table) if table.len() > 1000 => {
                let n = table.len();
                let mut g = litellm_state().write();
                g.table = table;
                g.origin = LitellmOrigin::Disk;
                tracing::info!(models = n, path = %path.display(), "litellm pricing loaded from disk cache");
            }
            Ok(table) => {
                tracing::warn!(
                    models = table.len(),
                    path = %path.display(),
                    "litellm disk cache too small; keeping embedded"
                );
            }
            Err(err) => {
                tracing::warn!(?err, path = %path.display(), "litellm disk cache parse failed; keeping embedded");
            }
        }
    }

    if litellm_refresh_disabled() {
        tracing::info!("litellm pricing auto-refresh disabled (SWARMX_DISABLE_LITELLM_REFRESH)");
        return;
    }

    tokio::spawn(async {
        match refresh_litellm_pricing_once().await {
            Ok(n) => tracing::info!(models = n, "litellm pricing refreshed from upstream"),
            Err(err) => tracing::warn!(
                ?err,
                "litellm pricing refresh failed; continuing with {}",
                litellm_state().read().origin.as_str()
            ),
        }
    });
}

async fn refresh_litellm_pricing_once() -> anyhow::Result<usize> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent(concat!("swarmx-server/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let raw: serde_json::Value = client.get(LITELLM_UPSTREAM_URL).send().await?.error_for_status()?.json().await?;
    let table = slim_litellm_catalog(&raw);
    let n = table.len();
    if n < 1000 {
        anyhow::bail!("upstream catalog too small ({n} models)");
    }
    save_litellm_disk_cache(&table)?;
    {
        let mut g = litellm_state().write();
        g.table = table;
        g.origin = LitellmOrigin::Refreshed;
    }
    Ok(n)
}

fn save_litellm_disk_cache(
    table: &std::collections::HashMap<String, LiteLlmEntry>,
) -> anyhow::Result<()> {
    use std::io::Write;
    let path = litellm_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Compact, stable: one model per line, sorted — same spirit as the update script.
    let mut keys: Vec<&String> = table.keys().collect();
    keys.sort();
    let mut body = String::from("{\n");
    for (i, k) in keys.iter().enumerate() {
        let e = &table[*k];
        let entry = serde_json::json!({
            "input": e.input,
            "output": e.output,
            "cache_read": e.cache_read,
            "cache_write": e.cache_write,
            "context_window": e.context_window,
        });
        // Drop null context_window for smaller diffs / match script shape loosely.
        let mut obj = entry.as_object().cloned().unwrap_or_default();
        if obj.get("context_window").is_some_and(|v| v.is_null()) {
            obj.remove("context_window");
        }
        body.push_str(&format!(
            "{}:{}",
            serde_json::to_string(k)?,
            serde_json::Value::Object(obj)
        ));
        if i + 1 < keys.len() {
            body.push(',');
        }
        body.push('\n');
    }
    body.push_str("}\n");
    let tmp = path.with_extension(crate::models_config::unique_tmp_ext());
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Convert BerriAI upstream (USD per token) → our slim map (USD per 1M tokens).
fn slim_litellm_catalog(raw: &serde_json::Value) -> std::collections::HashMap<String, LiteLlmEntry> {
    const M: f64 = 1_000_000.0;
    let round6 = |n: f64| (n * 1_000_000.0).round() / 1_000_000.0;
    let mut out = std::collections::HashMap::new();
    let Some(obj) = raw.as_object() else {
        return out;
    };
    for (name, spec) in obj {
        if name == "sample_spec" || !spec.is_object() {
            continue;
        }
        let Some(input_per_tok) = spec.get("input_cost_per_token").and_then(|v| v.as_f64()) else {
            continue;
        };
        if !input_per_tok.is_finite() {
            continue;
        }
        let output_per_tok = spec
            .get("output_cost_per_token")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        let cache_read_per_tok = spec
            .get("cache_read_input_token_cost")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        let cache_write_per_tok = spec
            .get("cache_creation_input_token_cost")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        let context_window = spec
            .get("max_input_tokens")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0)
            .map(|n| n as u32);
        out.insert(
            name.to_ascii_lowercase(),
            LiteLlmEntry {
                input: round6(input_per_tok * M),
                output: round6(output_per_tok * M),
                cache_read: round6(cache_read_per_tok * M),
                cache_write: round6(cache_write_per_tok * M),
                context_window,
            },
        );
    }
    out
}

fn litellm_table_len() -> usize {
    litellm_state().read().table.len()
}

fn litellm_origin() -> LitellmOrigin {
    litellm_state().read().origin
}

/// Normalise a model id toward a LiteLLM key: lowercase, drop a provider prefix
/// (`anthropic/claude-…`), and strip swarmx's 1M-context markers (`[1m]` /
/// `-1m`) that LiteLLM keys don't carry.
fn normalize_model(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    m = m.replace("[1m]", "");
    if let Some(stripped) = m.strip_suffix("-1m") {
        m = stripped.to_string();
    }
    if let Some(idx) = m.rfind('/') {
        m = m[idx + 1..].to_string();
    }
    m.trim().to_string()
}

/// Look a model up in the live LiteLLM table: exact lowercase first, then the
/// normalised form. None for models the catalog doesn't know.
fn litellm_lookup(model: &str) -> Option<LiteLlmEntry> {
    let g = litellm_state().read();
    let lower = model.trim().to_ascii_lowercase();
    g.table
        .get(&lower)
        .or_else(|| g.table.get(&normalize_model(model)))
        .copied()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PricingRule {
    id: String,
    provider: String,
    label: String,
    matchers: Vec<String>,
    context_window: Option<u32>,
    rates_usd_per_mtok: Rate,
    note: String,
}

#[derive(Deserialize, Serialize)]
pub struct PricingUpdate {
    rules: Vec<PricingRule>,
}

fn default_pricing_rules() -> Vec<PricingRule> {
    // Editable PRIMARY layer (shown in /usage). Matching is longest-needle-wins
    // (see `best_rule_match`) so specific ids beat short aliases. Prefer concrete
    // model markers over bare family names — a naked "opus" used to price every
    // current Opus id at the legacy $15/$75 band.
    //
    // Rates = published API list USD / 1M tokens (estimate only; subscription
    // spend is not a cash invoice). Unknown ids fall through to the LiteLLM
    // snapshot (auto-refreshed once per process from LiteLLM upstream; see
    // `spawn_litellm_pricing_refresh`). Or refresh the embed with
    // `scripts/update-litellm-pricing.mjs` at release time.
    vec![
        PricingRule {
            id: "anthropic-opus".into(),
            provider: "Anthropic".into(),
            label: "Claude Opus (current)".into(),
            matchers: vec![
                "opus-4-8".into(),
                "opus-4-7".into(),
                "opus-4-6".into(),
            ],
            context_window: Some(1_000_000),
            rates_usd_per_mtok: Rate {
                input: 5.0,
                output: 25.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
            note: "Current Opus list band. 1M-context markers ([1m]/-1m) force a 1M window."
                .into(),
        },
        PricingRule {
            id: "anthropic-opus-legacy".into(),
            provider: "Anthropic".into(),
            label: "Claude Opus (legacy)".into(),
            matchers: vec!["opus-4-1".into(), "claude-3-opus".into()],
            context_window: Some(200_000),
            rates_usd_per_mtok: Rate {
                input: 15.0,
                output: 75.0,
                cache_read: 1.5,
                cache_write: 18.75,
            },
            note: "Legacy Opus list band. Longer matchers beat current-band ids.".into(),
        },
        PricingRule {
            id: "anthropic-sonnet".into(),
            provider: "Anthropic".into(),
            label: "Claude Sonnet".into(),
            matchers: vec!["sonnet-4-6".into(), "sonnet".into()],
            context_window: Some(1_000_000),
            rates_usd_per_mtok: Rate {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            note: "Sonnet list band; sonnet-4-6 wins over bare sonnet when both match."
                .into(),
        },
        PricingRule {
            id: "anthropic-haiku".into(),
            provider: "Anthropic".into(),
            label: "Claude Haiku".into(),
            matchers: vec!["haiku".into()],
            context_window: Some(200_000),
            rates_usd_per_mtok: Rate {
                input: 1.0,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            note: String::new(),
        },
        PricingRule {
            id: "openai-gpt52".into(),
            provider: "OpenAI".into(),
            label: "GPT-5.2".into(),
            matchers: vec!["gpt-5.2".into(), "gpt5.2".into()],
            context_window: Some(272_000),
            rates_usd_per_mtok: Rate {
                input: 1.75,
                output: 14.0,
                cache_read: 0.175,
                cache_write: 0.0,
            },
            note: "Longer than gpt-5 so 5.2 is not mispriced as base gpt-5.".into(),
        },
        PricingRule {
            id: "openai-codex-gpt5".into(),
            provider: "OpenAI".into(),
            label: "GPT-5 / Codex family".into(),
            matchers: vec![
                "gpt-5.1".into(),
                "gpt5.1".into(),
                "gpt-5".into(),
                "gpt5".into(),
                "codex".into(),
                "o4".into(),
            ],
            context_window: Some(272_000),
            rates_usd_per_mtok: Rate {
                input: 1.25,
                output: 10.0,
                cache_read: 0.125,
                cache_write: 0.0,
            },
            note: "Approximation for gpt-5.1 / gpt-5 / codex CLI model ids.".into(),
        },
        PricingRule {
            id: "deepseek".into(),
            provider: "DeepSeek".into(),
            label: "DeepSeek (Reasonix)".into(),
            matchers: vec!["deepseek".into()],
            context_window: Some(1_000_000),
            rates_usd_per_mtok: Rate {
                // Alias for reasonix-style ids; exact `deepseek/…` keys prefer
                // LiteLLM when no longer primary needle wins (same length → first).
                input: 0.28,
                output: 0.42,
                cache_read: 0.028,
                cache_write: 0.0,
            },
            note: "Approximate alias; prefer LiteLLM for provider-prefixed ids when editable rules are cleared."
                .into(),
        },
    ]
}

/// Longest matching needle across all rules wins. Ties keep the first rule that
/// achieved that length (stable, editable-table order). Empty needles ignored.
fn best_rule_match<'a>(model: &str, rules: &'a [PricingRule]) -> Option<&'a PricingRule> {
    let m = model.to_ascii_lowercase();
    let mut best: Option<(&'a PricingRule, usize)> = None;
    for rule in rules {
        for needle in &rule.matchers {
            let n = needle.to_ascii_lowercase();
            if n.is_empty() || !m.contains(&n) {
                continue;
            }
            let len = n.len();
            match best {
                Some((_, best_len)) if len <= best_len => {}
                _ => best = Some((rule, len)),
            }
        }
    }
    best.map(|(rule, _)| rule)
}

fn pricing_config_path() -> PathBuf {
    // P1-39: HOME isn't set on Windows — fall back to USERPROFILE there so the
    // installed app writes/reads `~/.swarmx/pricing.json` instead of a
    // CWD-relative `.swarmx/pricing.json` (which, with CWD=`/` under the
    // sidecar, would make save/reset silently fail).
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".swarmx").join("pricing.json");
    }
    PathBuf::from(".swarmx/pricing.json")
}

fn validate_pricing_rules(rules: &[PricingRule]) -> Result<(), String> {
    if rules.is_empty() {
        return Err("pricing rules must not be empty".into());
    }
    for rule in rules {
        if rule.id.trim().is_empty() {
            return Err("pricing rule id must not be empty".into());
        }
        if rule.matchers.is_empty() || rule.matchers.iter().any(|m| m.trim().is_empty()) {
            return Err(format!(
                "pricing rule {} has an empty matcher (an empty matcher would match every model)",
                rule.id
            ));
        }
        let rates = [
            ("input", rule.rates_usd_per_mtok.input),
            ("output", rule.rates_usd_per_mtok.output),
            ("cache_read", rule.rates_usd_per_mtok.cache_read),
            ("cache_write", rule.rates_usd_per_mtok.cache_write),
        ];
        for (name, value) in rates {
            if !value.is_finite() || value < 0.0 {
                return Err(format!("pricing rule {} has invalid {name} rate", rule.id));
            }
        }
    }
    Ok(())
}

fn load_pricing_rules() -> (Vec<PricingRule>, &'static str) {
    let path = pricing_config_path();
    match std::fs::read_to_string(&path) {
        Ok(txt) => match serde_json::from_str::<PricingUpdate>(&txt) {
            Ok(update) if validate_pricing_rules(&update.rules).is_ok() => (update.rules, "user"),
            Ok(_) => {
                tracing::warn!(path = %path.display(), "pricing.json validation failed; using defaults");
                (default_pricing_rules(), "default")
            }
            Err(err) => {
                tracing::warn!(?err, path = %path.display(), "pricing.json parse failed; using defaults");
                (default_pricing_rules(), "default")
            }
        },
        Err(_) => (default_pricing_rules(), "default"),
    }
}

fn save_pricing_rules(rules: &[PricingRule]) -> anyhow::Result<()> {
    use std::io::Write;
    let path = pricing_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&PricingUpdate {
        rules: rules.to_vec(),
    })?;
    let tmp = path.with_extension(crate::models_config::unique_tmp_ext());
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Best-effort rate lookup. Primary layer: longest substring match against the
/// (user-editable) pricing rules. Fallback: exact/normalised match in the
/// embedded LiteLLM snapshot. Returns None only when neither knows the model
/// (cost contribution = 0, `priced` flips false).
fn rate_for(model: &str, rules: &[PricingRule]) -> Option<Rate> {
    if let Some(rule) = best_rule_match(model, rules) {
        return Some(rule.rates_usd_per_mtok);
    }
    litellm_lookup(model).map(|e| Rate {
        input: e.input,
        output: e.output,
        cache_read: e.cache_read,
        cache_write: e.cache_write,
    })
}

/// Best-effort context-window size (tokens). Surfaced in the Usage table so the
/// operator can eyeball headroom. Returns None for unknown models (UI shows "—").
fn context_window_for(model: &str, rules: &[PricingRule]) -> Option<u32> {
    let m = model.to_ascii_lowercase();
    if (m.contains("opus") || m.contains("sonnet")) && (m.contains("[1m]") || m.contains("-1m")) {
        return Some(1_000_000);
    }
    if let Some(cw) = best_rule_match(model, rules).and_then(|rule| rule.context_window) {
        return Some(cw);
    }
    litellm_lookup(model).and_then(|e| e.context_window)
}

fn cost_of(
    model: Option<&str>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    rules: &[PricingRule],
) -> Option<f64> {
    let r = rate_for(model.unwrap_or(""), rules)?;
    let per = |toks: i64, rate: f64| (toks as f64) / 1_000_000.0 * rate;
    Some(
        per(input, r.input)
            + per(output, r.output)
            + per(cache_read, r.cache_read)
            + per(cache_write, r.cache_write),
    )
}

#[derive(Serialize)]
struct ModelRow {
    model: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    events: i64,
    cost_usd: f64,
    priced: bool,
    /// The model's static context-window cap (tokens); null for unknown models.
    context_window: Option<u32>,
    /// Estimated peak context occupancy (tokens) — how full the window got.
    context_peak: i64,
}

#[derive(Deserialize)]
pub struct UsageQuery {
    /// Scope usage to one workspace; empty/absent = all workspaces.
    workspace_id: Option<String>,
}

/// One workspace's all-time estimated cost (USD) under the CURRENT pricing
/// table — the same arithmetic `usage_summary` reports, factored out so the
/// budget brake (crate::budget) trips on exactly the number the /usage page
/// shows. Returns `(cost_usd, all_priced)`; `all_priced = false` means some
/// models were unrecognised and the true spend is >= the returned estimate.
pub(crate) async fn workspace_cost_estimate(
    store: &swarmx_storage::Store,
    workspace_id: &str,
) -> anyhow::Result<(f64, bool)> {
    let (pricing_rules, _) = load_pricing_rules();
    let by_model = store
        .usage_by_model(Some(workspace_id.to_string()))
        .await?;
    let mut cost = 0.0f64;
    let mut all_priced = true;
    for m in &by_model {
        match cost_of(
            m.model.as_deref(),
            m.input_tokens,
            m.output_tokens,
            m.cache_read_tokens,
            m.cache_write_tokens,
            &pricing_rules,
        ) {
            Some(c) => cost += c,
            None => all_priced = false,
        }
    }
    Ok((cost, all_priced))
}

pub async fn usage_summary(
    State(state): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> impl IntoResponse {
    let store = &state.store;
    let (pricing_rules, _) = load_pricing_rules();
    let ws = q.workspace_id.filter(|s| !s.is_empty());
    // P1-37: don't unwrap_or_default() DB errors into empty stats — that renders
    // "你还没有用量" when the query actually failed. Surface a 500 so the page
    // shows a load error instead of a false "no usage yet".
    let usage_err = |e: anyhow::Error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response()
    };
    let by_model = match store.usage_by_model(ws.clone()).await {
        Ok(r) => r,
        Err(e) => return usage_err(e),
    };
    let by_day = match store.usage_by_day(90, ws.clone()).await {
        Ok(r) => r,
        Err(e) => return usage_err(e),
    };
    let by_agent = match store.usage_by_agent(ws).await {
        Ok(r) => r,
        Err(e) => return usage_err(e),
    };

    let mut models = Vec::with_capacity(by_model.len());
    let (mut t_in, mut t_out, mut t_cr, mut t_cw, mut t_ev, mut t_cost) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0f64);
    let mut all_priced = true;
    for m in &by_model {
        let cost = cost_of(
            m.model.as_deref(),
            m.input_tokens,
            m.output_tokens,
            m.cache_read_tokens,
            m.cache_write_tokens,
            &pricing_rules,
        );
        let priced = cost.is_some();
        if !priced {
            all_priced = false;
        }
        let cost = cost.unwrap_or(0.0);
        t_in += m.input_tokens;
        t_out += m.output_tokens;
        t_cr += m.cache_read_tokens;
        t_cw += m.cache_write_tokens;
        t_ev += m.events;
        t_cost += cost;
        models.push(ModelRow {
            model: m.model.clone(),
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
            cache_read_tokens: m.cache_read_tokens,
            cache_write_tokens: m.cache_write_tokens,
            events: m.events,
            cost_usd: cost,
            priced,
            context_window: m
                .model
                .as_deref()
                .and_then(|model| context_window_for(model, &pricing_rules)),
            context_peak: m.context_peak,
        });
    }

    Json(json!({
        "totals": {
            "input_tokens": t_in,
            "output_tokens": t_out,
            "cache_read_tokens": t_cr,
            "cache_write_tokens": t_cw,
            "events": t_ev,
            "cost_usd": t_cost,
            "priced": all_priced,
        },
        "by_model": models,
        "by_day": by_day,
        "by_agent": by_agent,
    }))
    .into_response()
}

pub async fn usage_pricing_get() -> impl IntoResponse {
    let (rules, source) = load_pricing_rules();
    Json(json!({
        "unit": "USD per 1M tokens",
        "source": source,
        "path": pricing_config_path(),
        "rules": rules,
        // Models no rule matches fall through to the LiteLLM catalog (embedded
        // at build, optionally refreshed once per process from GitHub).
        "fallback": {
            "source": "litellm",
            "origin": litellm_origin().as_str(),
            "models": litellm_table_len(),
            "auto_refresh": !litellm_refresh_disabled(),
            "cache_path": litellm_cache_path(),
        },
    }))
}

pub async fn usage_pricing_put(Json(update): Json<PricingUpdate>) -> Response {
    if let Err(error) = validate_pricing_rules(&update.rules) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    }
    match save_pricing_rules(&update.rules) {
        Ok(()) => usage_pricing_get().await.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

pub async fn usage_pricing_reset() -> Response {
    let path = pricing_config_path();
    match std::fs::remove_file(&path) {
        Ok(()) => usage_pricing_get().await.into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            usage_pricing_get().await.into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn litellm_snapshot_parses_and_is_populated() {
        // If include_str! or the JSON shape broke, this drops to 0 and every
        // fallback price silently disappears — guard against that.
        assert!(
            litellm_table_len() > 1000,
            "embedded snapshot should hold the full table, got {}",
            litellm_table_len()
        );
    }

    #[test]
    fn slim_litellm_catalog_converts_per_token_to_per_mtok() {
        let raw = serde_json::json!({
            "sample_spec": {"ignore": true},
            "claude-opus-4-8": {
                "input_cost_per_token": 0.000005,
                "output_cost_per_token": 0.000025,
                "cache_read_input_token_cost": 0.0000005,
                "cache_creation_input_token_cost": 0.00000625,
                "max_input_tokens": 1_000_000
            },
            "embed-only": { "input_cost_per_token": null }
        });
        let map = slim_litellm_catalog(&raw);
        let e = map.get("claude-opus-4-8").expect("slimmed");
        assert_eq!(e.input, 5.0);
        assert_eq!(e.output, 25.0);
        assert_eq!(e.cache_read, 0.5);
        assert_eq!(e.cache_write, 6.25);
        assert_eq!(e.context_window, Some(1_000_000));
        assert!(!map.contains_key("embed-only"));
        assert!(!map.contains_key("sample_spec"));
    }

    #[test]
    fn litellm_conversion_is_correct() {
        // Anchors the USD-per-token -> USD-per-1M-token (×1e6) conversion against
        // a known published price. opus: 1.5e-5/7.5e-5/1.5e-6/1.875e-5 per token.
        let opus = litellm_lookup("claude-opus-4-1").expect("opus in snapshot");
        assert_eq!(opus.input, 15.0);
        assert_eq!(opus.output, 75.0);
        assert_eq!(opus.cache_read, 1.5);
        assert_eq!(opus.cache_write, 18.75);
        assert_eq!(opus.context_window, Some(200_000));

        // A provider without cache-creation pricing must come through as 0, not absent.
        let codex = litellm_lookup("gpt-5-codex").expect("gpt-5-codex in snapshot");
        assert_eq!(codex.cache_write, 0.0);
    }

    #[test]
    fn validate_rejects_empty_matcher() {
        assert!(validate_pricing_rules(&default_pricing_rules()).is_ok());
        // An individually-empty matcher makes `contains("")` match every model —
        // reject it even beside a real matcher (regression: "one empty matcher
        // prices all models").
        let mut rules = default_pricing_rules();
        rules[0].matchers = vec!["".into(), "opus-4-8".into()];
        assert!(validate_pricing_rules(&rules).is_err());
        rules[0].matchers = vec!["   ".into()]; // whitespace-only is empty too
        assert!(validate_pricing_rules(&rules).is_err());
        rules[0].matchers = vec![]; // no matchers at all still rejected
        assert!(validate_pricing_rules(&rules).is_err());
    }

    #[test]
    fn rate_for_ignores_empty_needle() {
        // Defense-in-depth: even if an empty needle slipped past validation it
        // must not become a catch-all. One rule whose only matcher is "" —
        // an unknown model must stay unpriced, not silently inherit rates.
        let mut rules = default_pricing_rules();
        rules.truncate(1);
        rules[0].matchers = vec!["".into()];
        assert!(
            rate_for("zzz-nonexistent-model", &rules).is_none(),
            "empty needle must not catch-all an unknown model"
        );
    }

    #[test]
    fn normalize_strips_prefix_and_1m_markers() {
        assert_eq!(normalize_model("anthropic/claude-opus-4-1"), "claude-opus-4-1");
        assert_eq!(normalize_model("claude-opus-4-8[1m]"), "claude-opus-4-8");
        assert_eq!(normalize_model("Gemini-2.5-Pro-1m"), "gemini-2.5-pro");
    }

    #[test]
    fn primary_rules_win_over_litellm_fallback() {
        // Current Opus hits the hand-maintained rule; [1m] must not knock it
        // off the primary layer into a different band.
        let rules = default_pricing_rules();
        let r = rate_for("claude-opus-4-8[1m]", &rules).expect("opus priced");
        assert_eq!(r.input, 5.0);
        assert_eq!(r.output, 25.0);
    }

    #[test]
    fn longest_matcher_keeps_legacy_opus_and_gpt52_distinct() {
        let rules = default_pricing_rules();
        let legacy = rate_for("claude-opus-4-1", &rules).expect("legacy opus");
        assert_eq!(legacy.input, 15.0);
        assert_eq!(legacy.output, 75.0);

        let current = rate_for("claude-opus-4-8", &rules).expect("current opus");
        assert_eq!(current.input, 5.0);
        assert_eq!(current.output, 25.0);

        let g52 = rate_for("gpt-5.2-codex", &rules).expect("gpt-5.2");
        assert_eq!(g52.input, 1.75);
        assert_eq!(g52.output, 14.0);

        let g5 = rate_for("gpt-5-codex", &rules).expect("gpt-5");
        assert_eq!(g5.input, 1.25);
        assert_eq!(g5.output, 10.0);
    }

    #[test]
    fn unlisted_opus_id_uses_litellm_or_stays_unpriced() {
        // No bare "opus" catch-all: ids the primary table doesn't name must not
        // inherit a wrong family band.
        let rules = default_pricing_rules();
        let dated = rate_for("claude-opus-4-20250514", &rules).expect("snapshot has dated opus-4");
        assert_eq!(dated.input, 15.0);
        assert_eq!(dated.output, 75.0);
        assert!(rate_for("my-custom-opus-fork", &rules).is_none());
    }

    #[test]
    fn litellm_fallback_prices_models_no_rule_covers() {
        // gemini matches none of the primary needles — fallback prices it.
        let rules = default_pricing_rules();
        let r = rate_for("gemini-2.5-pro", &rules).expect("gemini priced via fallback");
        assert!(r.input > 0.0 && r.output > 0.0);
        assert!(context_window_for("gemini-2.5-pro", &rules).is_some());
    }

    #[test]
    fn truly_unknown_model_stays_unpriced() {
        let rules = default_pricing_rules();
        assert!(rate_for("totally-made-up-model-xyz", &rules).is_none());
        assert!(cost_of(Some("totally-made-up-model-xyz"), 100, 100, 0, 0, &rules).is_none());
    }

    #[test]
    fn cost_of_computes_the_actual_money() {
        // The pricing-LOOKUP tests above all verify which RATE matches a model,
        // but none asserted the arithmetic that turns tokens+rate into dollars.
        // Pin it against a hand-computed value so a swapped rate, a `+`→`*`
        // typo, or a drifted /1e6 divisor can't slip through.
        //
        // opus: input 15, output 75, cache_read 1.5, cache_write 18.75 (USD/Mtok).
        // 1M input + 1M output + 1M cache_read + 1M cache_write should be exactly
        // the sum of the four rates: 15 + 75 + 1.5 + 18.75 = 110.25.
        let rules = default_pricing_rules();
        let c = cost_of(Some("claude-opus-4-1"), 1_000_000, 1_000_000, 1_000_000, 1_000_000, &rules)
            .expect("opus is priced");
        assert!((c - 110.25).abs() < 1e-9, "expected 110.25, got {c}");

        // Each token class is weighted by ITS OWN rate, not lumped together:
        // 2M output @75 = 150; nothing else. Catches an input/output swap.
        let out_only = cost_of(Some("claude-opus-4-1"), 0, 2_000_000, 0, 0, &rules).unwrap();
        assert!((out_only - 150.0).abs() < 1e-9, "expected 150.0, got {out_only}");

        // Linear in token count and starts at zero.
        let half = cost_of(Some("claude-opus-4-1"), 500_000, 0, 0, 0, &rules).unwrap();
        assert!((half - 7.5).abs() < 1e-9, "expected 7.5, got {half}");
        let zero = cost_of(Some("claude-opus-4-1"), 0, 0, 0, 0, &rules).unwrap();
        assert_eq!(zero, 0.0);
    }
}
