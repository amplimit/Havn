-- havn — skill hybrid retrieval (spec §13 Phase 3, mirror of 0005).
--
-- Adds embedding storage to the `skills_index` table on the same
-- terms as memory: native-byte-order f32 BLOB + dim column, both
-- nullable so existing rows survive the migration unembedded. The
-- gateway-level provider handle and the Rust-side cosine sweep are
-- shared with the memory hybrid path; this migration is purely
-- storage.
--
-- Why no separate table: skills are already ~tens of rows per agent
-- (skills_index is one row per active SKILL.md). Brute-force cosine
-- against a few hundred 1536-d vectors is sub-millisecond and avoids
-- the portability/build cost of sqlite-vec — same trade-off as 0005.

ALTER TABLE skills_index ADD COLUMN embedding     BLOB;
ALTER TABLE skills_index ADD COLUMN embedding_dim INTEGER;
