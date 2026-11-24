-- havn — record the calls made without a credential (issue #232).
--
-- An [llm.endpoints] server that wants no key, such as a local Ollama, is
-- called without one; its calls still count against the agent's daily
-- token budget and show on the usage page, so credential_id may be NULL.
-- Rebuild the table, as 0007_credential_usages_keep_agent_deletes.sql
-- does: SQLite cannot drop a NOT NULL constraint in place, and no table
-- references credential_usages.

CREATE TABLE credential_usages_new (
    id                 TEXT    NOT NULL PRIMARY KEY,
    -- NULL for a call made without a credential.
    credential_id      TEXT    REFERENCES credentials(id) ON DELETE CASCADE,
    user_id            TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Nullable: gateway-direct LLM calls (e.g. the test endpoint) have no
    -- agent, and rows of deleted agents keep counting with agent_id NULL.
    agent_id           TEXT    REFERENCES agents(id) ON DELETE SET NULL,
    provider           TEXT    NOT NULL,
    model              TEXT    NOT NULL,
    tokens_in          INTEGER NOT NULL DEFAULT 0,
    tokens_out         INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    asked_model        TEXT    NOT NULL DEFAULT '',
    settled            INTEGER NOT NULL DEFAULT 1
) STRICT;

INSERT INTO credential_usages_new
    (id, credential_id, user_id, agent_id, provider, model, tokens_in, tokens_out, created_at,
     cache_read_tokens, cache_write_tokens, asked_model, settled)
    SELECT id, credential_id, user_id, agent_id, provider, model, tokens_in, tokens_out,
           created_at, cache_read_tokens, cache_write_tokens, asked_model, settled
    FROM credential_usages;

DROP TABLE credential_usages;
ALTER TABLE credential_usages_new RENAME TO credential_usages;

CREATE INDEX idx_credusage_user_time       ON credential_usages(user_id, created_at);
CREATE INDEX idx_credusage_credential_time ON credential_usages(credential_id, created_at);
CREATE INDEX idx_credusage_agent_time      ON credential_usages(agent_id, created_at);
CREATE INDEX idx_credusage_time            ON credential_usages(created_at);
