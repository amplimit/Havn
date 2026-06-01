-- Cross-agent query audit (spec §4.4 v0.7).
--
-- One row per AgentQuery, written by the gateway broker after the call
-- finishes (success, error, or timeout). Driven by ops audit + future
-- billing reports — "who is sending the most queries to whom, paying with
-- whose key, and how many tokens did it cost?"
--
-- Not on the hot path: the gateway already streams individual
-- credential_usages rows for each LLM call inside the query (those
-- attribute against `caller_user_id` thanks to LlmRequest.billing_user_id).
-- This table is the higher-level "the query happened" record.

CREATE TABLE cross_agent_queries (
    id                  TEXT NOT NULL PRIMARY KEY,                -- UUID v7 string
    caller_agent_id     TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    target_agent_id     TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    -- The user whose credentials were billed. v1 is always the caller's
    -- owner (same as target's, since same-owner is enforced); kept as a
    -- column so cross-owner extensions don't reshape the schema.
    caller_user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Truncated at write time (spec'd at 4 KiB) — the full prompt lives
    -- in the LLM proxy log if forensics ever need it. We keep just enough
    -- to identify the request.
    prompt_excerpt      TEXT NOT NULL,
    -- 'ok' | 'error' | 'timeout'. Mirror of AgentQueryOutcome.kind.
    outcome             TEXT NOT NULL CHECK (outcome IN ('ok','error','timeout')),
    -- Populated for 'error' / 'timeout', NULL for 'ok'.
    error_message       TEXT,
    -- Whether the caller asked for the full transcript. Useful when
    -- triaging "did the LLM see the whole thing?".
    include_transcript  INTEGER NOT NULL DEFAULT 0,
    started_at          TEXT NOT NULL,                            -- RFC3339
    finished_at         TEXT NOT NULL                             -- RFC3339
);

CREATE INDEX idx_cross_agent_queries_caller ON cross_agent_queries(caller_agent_id, started_at DESC);
CREATE INDEX idx_cross_agent_queries_target ON cross_agent_queries(target_agent_id, started_at DESC);
CREATE INDEX idx_cross_agent_queries_user   ON cross_agent_queries(caller_user_id, started_at DESC);
