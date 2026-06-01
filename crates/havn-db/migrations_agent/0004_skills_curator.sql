-- havn — skill telemetry + curator-friendly columns (spec §9.5).
--
-- Phase 1 / 2 shipped a flat skills_index that the runtime rebuilds on
-- every startup. Curator (§9.5) needs four extra signals:
--
-- 1. `source` — distinguish bundled (untouchable, ships with havn) from
--    workspace (potentially agent_created, curator-touchable).
-- 2. `pinned` — operator / agent says "don't touch this no matter what
--    the curator thinks". Mirrors spec §5.1 Skill.pinned in the gateway DB.
-- 3. `last_used_at` + `use_count` — equivalent of the memory table's
--    recall tracking. The curator's rule-based phase archives skills
--    that have been unused past a threshold.
-- 4. `archived_at` — soft-delete by the curator; the SKILL.md file gets
--    moved to `.archive/<name>/SKILL.md` on disk in parallel. NULL =
--    active and surfaced in retrieval.

ALTER TABLE skills_index ADD COLUMN source TEXT NOT NULL DEFAULT 'workspace'
    CHECK (source IN ('bundled', 'workspace'));

ALTER TABLE skills_index ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0
    CHECK (pinned IN (0, 1));

ALTER TABLE skills_index ADD COLUMN last_used_at TEXT;
ALTER TABLE skills_index ADD COLUMN use_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE skills_index ADD COLUMN archived_at TEXT;

-- Curator scans by source + active + last-used. Partial index keeps the
-- scan cheap once the table accumulates archived rows.
CREATE INDEX idx_skills_curatable ON skills_index(source, pinned, archived_at, last_used_at)
    WHERE archived_at IS NULL AND source = 'workspace';
