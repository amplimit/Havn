-- havn — memory hybrid retrieval (spec §9.4 v0.7).
--
-- Adds embedding storage to the agent-side `memory` table. Vectors
-- are stored as BLOB bytes (4 bytes per float, native byte order,
-- via `bytemuck::cast_slice`) — pure-Rust KNN at query time, no
-- sqlite-vec dependency. Typed memory is at the hundreds-of-rows
-- scale per agent (it stores facts about the user, not full chat
-- history — conversations live elsewhere), so brute-force cosine
-- over a `Vec<Vec<f32>>` is sub-millisecond and avoids the
-- portability + build-complexity cost of a SQLite C extension.
--
-- `embedding_dim` is stored alongside so the runtime can validate
-- that the configured embedding provider's dimension matches what
-- was written on a per-row basis (different rows may have been
-- written under different providers if the operator switched). On
-- mismatch we just fall through to FTS5-only for that row and log;
-- explicit `havn memory reindex` (Stage 2) reconciles.
--
-- Both columns nullable: existing rows have no embedding until they
-- are either rewritten by `memory::remember` (which captures the
-- vector when an embedder is wired) or backfilled by reindex.

ALTER TABLE memory ADD COLUMN embedding     BLOB;
ALTER TABLE memory ADD COLUMN embedding_dim INTEGER;
