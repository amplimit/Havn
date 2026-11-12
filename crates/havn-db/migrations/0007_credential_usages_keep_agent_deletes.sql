-- havn — keep LLM usage when an agent is deleted.
--
-- credential_usages.agent_id was ON DELETE CASCADE, so deleting an agent
-- erased its usage from the credential-wide and per-user daily token caps
-- (spend the budget, delete the agent, start over) and from the usage
-- history. Rebuild the table with ON DELETE SET NULL, same dance as
-- 0003_users_simplify.sql. No table references credential_usages, so
-- dropping it touches nothing else.

CREATE TABLE credential_usages_new (
    id             TEXT    NOT NULL PRIMARY KEY,
    credential_id  TEXT    NOT NULL REFERENCES credentials(id) ON DELETE CASCADE,
    user_id        TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Nullable: gateway-direct LLM calls (e.g. the test endpoint) have no
    -- agent, and rows of deleted agents keep counting with agent_id NULL.
    agent_id       TEXT    REFERENCES agents(id) ON DELETE SET NULL,
    provider       TEXT    NOT NULL,
    model          TEXT    NOT NULL,
    tokens_in      INTEGER NOT NULL DEFAULT 0,
    tokens_out     INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

INSERT INTO credential_usages_new
    (id, credential_id, user_id, agent_id, provider, model, tokens_in, tokens_out, created_at)
    SELECT id, credential_id, user_id, agent_id, provider, model, tokens_in, tokens_out, created_at
    FROM credential_usages;

DROP TABLE credential_usages;
ALTER TABLE credential_usages_new RENAME TO credential_usages;

CREATE INDEX idx_credusage_user_time       ON credential_usages(user_id, created_at);
CREATE INDEX idx_credusage_credential_time ON credential_usages(credential_id, created_at);
CREATE INDEX idx_credusage_agent_time      ON credential_usages(agent_id, created_at);
