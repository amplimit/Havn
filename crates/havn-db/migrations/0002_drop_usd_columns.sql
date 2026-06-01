-- havn — v0.6 USD removal (spec §7.3).
--
-- Earlier drafts had a model-pricing table driving estimated_usd
-- accounting per LLM call and max_usd_per_day budget enforcement. Both
-- are out: pricing changes weekly, a stale table in havn's binary is
-- worse than no table, and operators that care about $ already have
-- their own analytics. Tokens are the only first-class budget unit
-- havn enforces.
--
-- SQLite ≥ 3.35 supports ALTER TABLE DROP COLUMN; we rely on the same
-- ≥ 3.37 STRICT-tables baseline as the rest of the schema, so this is
-- safe.

ALTER TABLE credential_usages DROP COLUMN estimated_usd;

-- Also drop the gateway-side curator_reports column that was only used
-- by the never-implemented gateway-side curator. Phase 2 ships the
-- curator inside the runtime; reports live as files in
-- workspace/.curator/<ts>.md, not in this table. Leaving the unused
-- columns around invites confusion about which side does what.
--
-- (We keep the curator_reports table itself in case a future cross-
-- agent rollup view wants gateway-side aggregation; just drop the
-- payload columns nobody writes.)

-- Drop the never-written columns from skills (the curator's
-- absorbed_into pointer was cut in v0.6 — see spec §9.5; the
-- task_duration_ms field on skill_usages was a phantom metric).
ALTER TABLE skills DROP COLUMN absorbed_into;
ALTER TABLE skill_usages DROP COLUMN task_duration_ms;
