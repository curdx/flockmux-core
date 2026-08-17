-- 0030_workspace_budget: per-workspace budget brake (soft but real).
--
-- The product's #1 trust gap: a swarm can silently burn subscription quota
-- and the user only finds out afterwards. This migration gives a workspace an
-- OPTIONAL all-time estimated-spend cap (`budget_usd`; NULL or <= 0 =
-- unlimited, matching the all-time nature of /api/usage totals) plus the
-- brake's persisted state:
--
--   - budget_exceeded_at / budget_exceeded_cost_usd: the trip marker. Non-NULL
--     = the brake is ON: spawns and new turn deliveries for this workspace are
--     refused (fail-closed) until the budget is raised above the estimate or
--     cleared. The trip-time estimate is kept for honest display ("trip 时估算
--     $X") — every number here is an ESTIMATE from transcript scraping, never
--     the subscription invoice.
--   - workspace_budget_pauses: exactly WHICH agents the brake paused. The
--     pause flag itself is in-memory on the registry slot, so without this
--     table a later lift couldn't tell brake-paused agents from operator-
--     paused ones (operator-paused must stay paused across a lift).
--
-- ALTER TABLE ADD COLUMN: SQLite only adds nullable columns without a table
-- rewrite, so all three workspaces columns are nullable; NULL = unset.

INSERT INTO schema_version VALUES (30);

ALTER TABLE workspaces ADD COLUMN budget_usd REAL;
ALTER TABLE workspaces ADD COLUMN budget_exceeded_at INTEGER;
ALTER TABLE workspaces ADD COLUMN budget_exceeded_cost_usd REAL;

CREATE TABLE workspace_budget_pauses (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id),
    agent_id     TEXT NOT NULL,
    paused_at    INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, agent_id)
);
