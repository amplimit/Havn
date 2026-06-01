-- havn — memory v2: recall tracking + supersedes chain (spec §9.4).
--
-- Rationale (drawn from auditing OpenClaw's "dreaming" promotion scoring
-- and Hermes's curator pass):
--
-- - Time-only TTL is a known wrong heuristic. A 30-day-old event that
--   the agent has cited 5 times in the last week is NOT stale — it's a
--   load-bearing fact in the live conversation. OpenClaw's
--   `recallCount` / `lastRecalledAt` is the standard fix; we adopt it.
--
-- - When the user changes their mind ("I'm not at X anymore — at Y now"),
--   `remember()` overwrites the value but the audit trail loses the
--   reason for the change. `supersedes_id` lets the dashboard render
--   the chain so the user can see "agent used to think X, now thinks Y,
--   the X row was archived on 2026-05-03 by user_told write".

ALTER TABLE memory ADD COLUMN recall_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE memory ADD COLUMN last_recalled_at TEXT;

-- supersedes_id: when set, this active row replaced an archived row
-- (which itself stays in the table for audit). Self-referential FK with
-- ON DELETE SET NULL because we never actually DELETE memory, but the
-- ON DELETE behavior is a sane fallback if a future cleanup script does.
ALTER TABLE memory ADD COLUMN supersedes_id TEXT REFERENCES memory(id) ON DELETE SET NULL;

-- The aging pass needs a fast lookup for "rows whose effective freshness
-- (max of updated_at and last_recalled_at) is older than ttl_days". Index
-- includes last_recalled_at so the partial index is useful for the case
-- where a row was rewritten long ago but recalled recently.
CREATE INDEX idx_memory_freshness ON memory(updated_at, last_recalled_at)
    WHERE archived_at IS NULL AND ttl_days IS NOT NULL;

-- For the dashboard's "audit trail" view (next vertical): list archived
-- rows by what superseded them so we can render a chain.
CREATE INDEX idx_memory_supersedes ON memory(supersedes_id)
    WHERE supersedes_id IS NOT NULL;
