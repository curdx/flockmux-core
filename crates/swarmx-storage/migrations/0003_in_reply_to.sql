-- 0003_in_reply_to: thread messages via parent pointer.
--
-- ALTER TABLE ADD COLUMN with no DEFAULT is safe + back-compat: existing rows
-- get NULL, no rewrite. Note foreign_keys IS ON in this DB — connection.rs
-- primes every pooled connection (thought_traces in 0021 relies on live
-- cascades). This ref defaults to NO ACTION, so it rejects deleting a parent
-- message that still has replies; prune_expired's `id NOT IN (SELECT
-- in_reply_to ...)` guard exists precisely to stay FK-safe. No delete cascade
-- is wanted here — the ref documents the relationship for tooling.
--
-- The partial index keeps lookups for "replies to message N" fast without
-- bloating storage for the common case (in_reply_to IS NULL).

INSERT INTO schema_version VALUES (3);

ALTER TABLE messages ADD COLUMN in_reply_to INTEGER REFERENCES messages(id);

CREATE INDEX idx_messages_in_reply_to ON messages(in_reply_to)
    WHERE in_reply_to IS NOT NULL;
