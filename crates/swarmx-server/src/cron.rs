//! Minimal 5-field cron matcher (`min hour dom month dow`), UTC.
//!
//! No new dependency: we decompose a unix timestamp into calendar fields with
//! Howard Hinnant's civil-from-days algorithm. Schedules are evaluated in **UTC**
//! (documented in the UI) — keeping a tz database out of the build. Supports per
//! field: `*`, `*/n`, `a`, `a-b`, `a,b` and comma-combinations. `dow` is 0–6 with
//! 0 = Sunday (7 also accepted as Sunday).

/// Calendar fields of an instant (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    pub minute: u32, // 0..=59
    pub hour: u32,   // 0..=23
    pub dom: u32,    // 1..=31
    pub month: u32,  // 1..=12
    pub dow: u32,    // 0..=6, 0=Sunday
}

/// Decompose unix **seconds** into UTC calendar fields.
pub fn fields_from_unix(secs: i64) -> Fields {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let minute = ((secs_of_day / 60) % 60) as u32;
    let hour = (secs_of_day / 3600) as u32;
    let (_y, month, dom) = civil_from_days(days);
    // 1970-01-01 was Thursday. With 0=Sunday: (days + 4) mod 7.
    let dow = (days.rem_euclid(7) as u32 + 4) % 7;
    Fields {
        minute,
        hour,
        dom,
        month,
        dow,
    }
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Does one cron field match `value`? `min`/`max` bound `*` and `*/n` expansion.
fn field_matches(field: &str, value: u32, min: u32, max: u32) -> bool {
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part == "*" {
            return true;
        }
        if let Some(step_s) = part.strip_prefix("*/") {
            if let Ok(step) = step_s.parse::<u32>() {
                if step != 0 && (value.saturating_sub(min)) % step == 0 {
                    return true;
                }
            }
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                let (lo, hi) = (a.max(min), b.min(max));
                if value >= lo && value <= hi {
                    return true;
                }
            }
            continue;
        }
        if let Ok(n) = part.parse::<u32>() {
            if n == value {
                return true;
            }
        }
    }
    false
}

/// True if `expr` (5 fields) matches the given instant. Malformed expr → false.
pub fn matches(expr: &str, f: Fields) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    matches_parts(&parts, f)
}

/// Match against pre-split fields. Hot-loop callers (`next_after`) split once and
/// reuse this, avoiding a `Vec<&str>` allocation per candidate minute.
fn matches_parts(parts: &[&str], f: Fields) -> bool {
    date_matches(parts, f)
        && field_matches(parts[0], f.minute, 0, 59)
        && field_matches(parts[1], f.hour, 0, 23)
}

/// Match only the DATE fields (dom/month/dow). If a day fails this, no time of
/// that day can match — `next_after` uses it to skip a whole day at once instead
/// of probing all 1440 minutes (turns a never-firing expr's ~527k-minute scan
/// into ~366 day checks). Same AND-semantics as `matches_parts` so the two agree.
fn date_matches(parts: &[&str], f: Fields) -> bool {
    // dow: accept 7 as Sunday by normalising the value side too.
    let dow_field = parts[4];
    let dow_ok =
        field_matches(dow_field, f.dow, 0, 6) || (f.dow == 0 && field_matches(dow_field, 7, 0, 7));
    field_matches(parts[2], f.dom, 1, 31) && field_matches(parts[3], f.month, 1, 12) && dow_ok
}

/// Validate one field against its [min,max] range. Accepts `*`, `*/n` (n>0),
/// `a-b` (both in range, a≤b), `a` (in range), and comma lists of those. A
/// number outside the range (e.g. minute `99`) is rejected — the old check only
/// tested that tokens *parsed*, so garbage like `99 99 99 99 99` was stored and
/// then silently never fired.
fn field_valid(field: &str, min: u32, max: u32) -> bool {
    field.split(',').all(|p| {
        let p = p.trim();
        if p.is_empty() {
            return false;
        }
        if p == "*" {
            return true;
        }
        if let Some(s) = p.strip_prefix("*/") {
            return s.parse::<u32>().map(|n| n != 0).unwrap_or(false);
        }
        if let Some((a, b)) = p.split_once('-') {
            return match (a.parse::<u32>(), b.parse::<u32>()) {
                (Ok(a), Ok(b)) => a >= min && a <= max && b >= min && b <= max && a <= b,
                _ => false,
            };
        }
        p.parse::<u32>()
            .map(|n| n >= min && n <= max)
            .unwrap_or(false)
    })
}

/// Range-aware validity check for a 5-field expression (used by the REST layer to
/// reject garbage before storing, and by the live preview).
pub fn is_valid(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    field_valid(parts[0], 0, 59)
        && field_valid(parts[1], 0, 23)
        && field_valid(parts[2], 1, 31)
        && field_valid(parts[3], 1, 12)
        && field_valid(parts[4], 0, 7) // dow 0–7 (7 = Sunday)
}

/// Next unix **seconds** (minute-aligned) strictly after `from_secs` at which
/// `expr` fires, searching up to ~366 days. `offset_min` is minutes east of UTC
/// the expression is written in (0 = UTC): the returned instant is in UTC, but
/// the expression's fields are matched against `t + offset` so `0 9 * * *` with
/// `offset_min = 480` resolves to the UTC instant whose **local** wall-clock is
/// 09:00. `None` if the expr is invalid or has no occurrence in that window
/// (e.g. an impossible `0 0 30 2 *`). Reuses `matches` so the preview agrees
/// exactly with the scheduler.
pub fn next_after(expr: &str, from_secs: i64, offset_min: i32) -> Option<i64> {
    if !is_valid(expr) {
        return None;
    }
    // Split the 5 fields ONCE (is_valid guaranteed exactly 5) and reuse them.
    let parts: Vec<&str> = expr.split_whitespace().collect();
    let shift = offset_min as i64 * 60;
    let mut t = (from_secs / 60 + 1) * 60; // next whole minute
    let limit = t + 366 * 86_400;
    while t <= limit {
        let f = fields_from_unix(t + shift);
        // If the day's date fields can't match, jump straight to the next local
        // midnight rather than probing 1440 dead minutes. For a never-firing
        // expr (e.g. `0 0 30 2 *`) this is ~366 iterations instead of ~527k.
        if !date_matches(&parts, f) {
            let local = t + shift;
            t = (local.div_euclid(86_400) + 1) * 86_400 - shift; // next local midnight, back to UTC
            continue;
        }
        if field_matches(parts[1], f.hour, 0, 23) && field_matches(parts[0], f.minute, 0, 59) {
            return Some(t);
        }
        t += 60;
    }
    None
}

// ── action + scheduler ──────────────────────────────────────────────────────

use crate::AppState;
use axum::extract::State;
use axum::Json;
use swarmx_protocol::rest::RunSpellRequest;
use swarmx_storage::CronJobRecord;
use swarmx_swarm::NewMessage;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fire one job NOW: deliver its prompt to the workspace's orchestrator
/// (message + wake), then stamp `last_run_at`. When no orchestrator is alive —
/// the common state for an idle or just-rebooted workspace — revive one on
/// demand (the `init` spell, same path the chat composer uses) so a scheduled
/// fire actually runs instead of silently skipping. Shared by the scheduler and
/// the manual `POST /run`.
pub async fn run_job(state: &AppState, job: &CronJobRecord) -> Result<(), String> {
    // Budget brake: a cron fire is a turn delivery (message + wake, possibly
    // an orchestrator revive). While the workspace's brake is on, skip with
    // the honest error — a silently-dropped schedule would look broken, and
    // running it would burn the capped budget.
    if let Some(msg) =
        crate::budget::exceeded_error_for_workspace(&state.store, &job.workspace_id).await
    {
        return Err(msg);
    }
    let agents = state.store.list_agents().await.map_err(|e| e.to_string())?;
    let live_orch = agents.into_iter().find(|a| {
        a.killed_at.is_none()
            && a.workspace_id.as_deref() == Some(job.workspace_id.as_str())
            && a.role == "orchestrator"
    });
    let orch_id = match live_orch {
        Some(o) => o.id,
        None => revive_orchestrator(state, job).await?,
    };
    let now = now_ms();
    state
        .swarm
        .send_message(NewMessage {
            from_agent: "cron".into(),
            to_agent: orch_id.clone(),
            kind: "note".into(),
            body: job.prompt.clone(),
            sent_at: now,
            in_reply_to: None,
            meta: Some(serde_json::json!({ "subtype": "cron", "job": job.name })),
        })
        .await
        .map_err(|e| e.to_string())?;
    let _ = crate::wake::deliver_manual_wake(&state.swarm, &state.registry, &state.server_url, &orch_id).await;
    let _ = state.store.touch_cron_run(job.id.clone(), now).await;
    Ok(())
}

/// Spawn a fresh orchestrator for the job's workspace (the `init` spell) and
/// return its agent id. Runs in the workspace's main direction; the bootstrap
/// detects the existing ledger and short-circuits, then reads the cron message
/// we deliver right after. Mirrors the chat "send-to-revive" launch.
async fn revive_orchestrator(state: &AppState, job: &CronJobRecord) -> Result<String, String> {
    let ws = state
        .store
        .get_workspace_by_id(job.workspace_id.clone())
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "workspace not found".to_string())?;
    let req = RunSpellRequest {
        name: "init".into(),
        task: String::new(),
        workspace_dir: Some(ws.cwd.clone()),
        workspace_id: Some(job.workspace_id.clone()),
        caller_agent_id: None,
        thread_id: None, // → resolves to the workspace's main direction
        // Preserve the captain engine when a cron job revives the orchestrator.
        captain_cli: crate::routes::rest::last_orchestrator_cli(state, &job.workspace_id).await,
    };
    let resp = crate::routes::rest::run_spell(State(state.clone()), Json(req))
        .await
        .map_err(|(code, body)| format!("revive orchestrator failed ({code}): {}", body.0))?
        .0;
    resp.agents
        .iter()
        .find(|a| a.role == "orchestrator")
        .or_else(|| resp.agents.first())
        .map(|a| a.agent_id.clone())
        .ok_or_else(|| "init spell spawned no orchestrator".to_string())
}

/// Background scheduler: every 30s, fire any enabled job whose 5-field cron
/// expression matches the current UTC minute and hasn't already run this
/// minute (`last_run_at` dedup). Runs for the process lifetime.
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let now = now_ms();
            let now_secs = now / 1000;
            let cur_min = now / 60_000;
            let jobs = match state.store.list_cron_jobs().await {
                Ok(j) => j,
                Err(_) => continue,
            };
            for job in jobs {
                // Decompose `now + offset` per job so the expression is matched
                // against the job's local wall-clock (offset 0 = UTC).
                let f = fields_from_unix(now_secs + job.tz_offset_minutes as i64 * 60);
                if !job.enabled || !matches(&job.cron_expr, f) {
                    continue;
                }
                if job.last_run_at.map(|l| l / 60_000) == Some(cur_min) {
                    continue; // already fired this minute
                }
                match run_job(&state, &job).await {
                    Ok(()) => tracing::info!(job = %job.name, "cron: fired"),
                    Err(e) => tracing::debug!(job = %job.name, %e, "cron: skipped"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompose_known_instant() {
        // 2021-01-01 00:00:00 UTC = 1609459200; a Friday (dow=5).
        let f = fields_from_unix(1_609_459_200);
        assert_eq!((f.minute, f.hour, f.dom, f.month, f.dow), (0, 0, 1, 1, 5));
    }

    #[test]
    fn star_matches_anything() {
        let f = fields_from_unix(1_609_459_200);
        assert!(matches("* * * * *", f));
    }

    #[test]
    fn exact_minute_hour() {
        let f = Fields {
            minute: 30,
            hour: 9,
            dom: 15,
            month: 6,
            dow: 1,
        };
        assert!(matches("30 9 * * *", f));
        assert!(!matches("31 9 * * *", f));
        assert!(!matches("30 10 * * *", f));
    }

    #[test]
    fn step_and_range_and_list() {
        let f = Fields {
            minute: 0,
            hour: 14,
            dom: 10,
            month: 6,
            dow: 3,
        };
        assert!(matches("*/15 * * * *", f)); // 0 % 15 == 0
        assert!(!matches("*/15 * * * *", Fields { minute: 7, ..f }));
        assert!(matches("0 9-17 * * *", f)); // 14 in 9..17
        assert!(matches("0 14 * * 1,3,5", f)); // dow 3 in list
        assert!(!matches("0 14 * * 0,6", f)); // weekend only
    }

    #[test]
    fn dow_sunday_seven_or_zero() {
        // 2021-01-03 = Sunday. dow should match both 0 and 7.
        let f = fields_from_unix(1_609_459_200 + 2 * 86_400);
        assert_eq!(f.dow, 0);
        assert!(matches("* * * * 0", f));
        assert!(matches("* * * * 7", f));
    }

    #[test]
    fn validity() {
        assert!(is_valid("*/5 * * * *"));
        assert!(is_valid("0 9 * * 1-5"));
        assert!(is_valid("0 0 1 1 0"));
        assert!(!is_valid("0 9 * *")); // 4 fields
        assert!(!is_valid("xx 9 * * *"));
    }

    #[test]
    fn validity_rejects_out_of_range() {
        assert!(!is_valid("99 99 99 99 99")); // the QA bug: parsed but never fires
        assert!(!is_valid("60 0 * * *")); // minute 60
        assert!(!is_valid("0 24 * * *")); // hour 24
        assert!(!is_valid("0 0 0 1 0")); // dom 0
        assert!(!is_valid("0 0 1 13 0")); // month 13
        assert!(!is_valid("0 0 1 1 8")); // dow 8
        assert!(!is_valid("5-1 * * * *")); // reversed range
    }

    #[test]
    fn next_after_finds_upcoming() {
        // 2021-01-01 00:00:00 UTC (Friday). Next "0 0 * * *" is the next midnight.
        let base = 1_609_459_200;
        assert_eq!(next_after("0 0 * * *", base, 0), Some(base + 86_400));
        // Every minute → the very next minute.
        assert_eq!(next_after("* * * * *", base + 10, 0), Some(base + 60));
        // Impossible date → no occurrence within a year.
        assert_eq!(next_after("0 0 30 2 *", base, 0), None);
        // Invalid → None.
        assert_eq!(next_after("99 99 99 99 99", base, 0), None);
    }

    #[test]
    fn offset_shifts_hour() {
        // 2021-01-01 00:00 UTC = 08:00 local in UTC+8 (offset 480).
        let base = 1_609_459_200;
        // "every day 09:00 local" fires at 01:00 UTC the same day.
        assert_eq!(next_after("0 9 * * *", base, 480), Some(base + 3_600));
        // UTC-5 (offset -300): 09:00 local = 14:00 UTC.
        assert_eq!(next_after("0 9 * * *", base, -300), Some(base + 14 * 3_600));
        // Sanity: offset 0 is the plain-UTC behaviour.
        assert_eq!(next_after("0 9 * * *", base, 0), Some(base + 9 * 3_600));
    }

    #[test]
    fn offset_rolls_day_of_week_correctly() {
        // 2021-01-04 is a Monday. base = its 00:00 UTC.
        let monday = 1_609_459_200 + 3 * 86_400;
        let sunday_2200 = monday - 2 * 3_600; // 2021-01-03 22:00 UTC (a Sunday)
        // UTC-5: "Sunday 22:00 local" = Monday 03:00 UTC. The matcher must read
        // the *local* dow (Sunday=0), not the UTC dow (Monday) — the rollover a
        // naive hour-shift would get wrong.
        assert_eq!(
            next_after("0 22 * * 0", sunday_2200, -300),
            Some(monday + 3 * 3_600),
        );
        // And the live scheduler agrees: at Monday 03:00 UTC with offset -300,
        // the decomposed fields are Sunday 22:00.
        let f = fields_from_unix((monday + 3 * 3_600) + (-300) * 60);
        assert_eq!((f.hour, f.dow), (22, 0));
        assert!(matches("0 22 * * 0", f));
    }

    #[test]
    fn next_after_day_skip_finds_yearly_and_weekly() {
        let base = 1_609_459_200; // 2021-01-01 00:00 UTC (Friday)
        // Yearly: next "0 0 1 7 *" is July 1 00:00 — reached via day-skip, not a
        // 6-month minute crawl. Verify it lands on the exact field combination.
        let jul1 = next_after("0 0 1 7 *", base, 0).unwrap();
        let f = fields_from_unix(jul1);
        assert_eq!((f.month, f.dom, f.hour, f.minute), (7, 1, 0, 0));
        assert!(jul1 > base && jul1 - base < 366 * 86_400);
        // Weekly: next Wednesday 14:30. Jan 6 2021 is the first Wednesday.
        let wed = next_after("30 14 * * 3", base, 0).unwrap();
        assert_eq!(wed, base + 5 * 86_400 + 14 * 3_600 + 30 * 60);
        let wf = fields_from_unix(wed);
        assert_eq!((wf.dow, wf.hour, wf.minute), (3, 14, 30));
    }
}
