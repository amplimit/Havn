-- havn — typed memory (spec §9.4 layer 3).
--
-- Phase 1 shipped a flat key/value `memory` table. Phase 2 introduces
-- the typing the user actually wants: facts about *who they are*,
-- *what they prefer*, *what project they're on*, and *what just
-- happened* are different in kind and need different lifetimes.
--
-- ALTER TABLE works with STRICT mode in SQLite ≥ 3.37 as long as the
-- new columns are typed and either nullable or have an explicit default.

-- 1. kind — what flavour of fact this is.
--    identity   — stable facts about the user (name, role, languages). Never auto-aged.
--    preference — durable preferences and corrections. Never auto-aged.
--    project    — current-work facts, may go stale (default ttl 90 days).
--    event      — discrete time-stamped incidents (default ttl 30 days).
--
--    Stored as TEXT with a CHECK constraint instead of an enum because
--    SQLite doesn't have native enums and TEXT + CHECK gives the same
--    forward-compat for a column that may grow.
ALTER TABLE memory ADD COLUMN kind TEXT NOT NULL DEFAULT 'preference'
    CHECK (kind IN ('identity', 'preference', 'project', 'event'));

-- 2. source — who put this fact here.
--    user_told      — the user said it directly.
--    agent_inferred — the agent guessed it from context.
--
--    Lets the dashboard (and future curator) treat user-told facts as
--    authoritative and agent-inferred ones as candidates for verification.
ALTER TABLE memory ADD COLUMN source TEXT NOT NULL DEFAULT 'agent_inferred'
    CHECK (source IN ('user_told', 'agent_inferred'));

-- 3. ttl_days — after this many days unused, the aging pass marks
--    archived_at. NULL = never auto-expire (used by identity / preference).
ALTER TABLE memory ADD COLUMN ttl_days INTEGER;

-- 4. archived_at — soft-delete. Set by `memory_forget` (explicit) or
--    by the daily aging pass (TTL expiry). Active rows are
--    `archived_at IS NULL`. We never DELETE memory rows so the dashboard
--    can show an audit trail of "what the agent has known about you".
ALTER TABLE memory ADD COLUMN archived_at TEXT;

CREATE INDEX idx_memory_kind_active ON memory(kind, archived_at)
    WHERE archived_at IS NULL;

-- The aging pass walks active event/project rows by `updated_at` to find
-- candidates whose age exceeds `ttl_days`. Index helps that scan stay cheap
-- once the table accumulates thousands of events.
CREATE INDEX idx_memory_aging ON memory(updated_at)
    WHERE archived_at IS NULL AND ttl_days IS NOT NULL;
