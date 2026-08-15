-- 0029_agent_shim_pid: the agent row learns its process identity.
--
-- Until now the agents table tracked only UI lifecycle (spawned/ready/exited/
-- killed). A server crash or SIGKILL leaves the real process tree — shim →
-- claude/codex → its descendants — reparented to init and still burning
-- subscription quota, while `mark_orphan_agents_killed` merely flipped the DB
-- row. Reaping those orphans for real at the next boot needs two facts the
-- row never stored: the shim's OS pid (probe target) and its process-group id
-- (kill target — portable-pty setsid's the shim, so pgid == shim pid and the
-- whole CLI tree shares it).
--
-- shim_pid:  OS process id of the `swarmx-shim` child. NULL for rows spawned
--   before this column existed (they can never be process-reaped — the pid is
--   gone — so the startup reaper skips them; marking their rows killed is
--   still all we can do).
-- shim_pgid: unix process-group id of the shim, resolved via getpgid() when
--   the reaper first records the pid. NULL on Windows, which has no process
--   groups — the reaper fells the tree by pid via `taskkill /T` there.
--
-- ALTER TABLE ADD COLUMN with no DEFAULT is safe + back-compat: existing rows
-- get NULL, no table rewrite.

INSERT INTO schema_version VALUES (29);

ALTER TABLE agents ADD COLUMN shim_pid INTEGER;
ALTER TABLE agents ADD COLUMN shim_pgid INTEGER;
