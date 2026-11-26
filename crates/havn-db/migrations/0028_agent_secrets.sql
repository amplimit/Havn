-- havn — secrets for an agent's tools (issue #233).
--
-- One row per secret of an agent, set by its owner on the dashboard, or
-- asked for by the agent with `secret_request` and not set yet. `value` is
-- age ciphertext made with the gateway's HAVN_AGE_KEY, as credentials'
-- `api_key` is; NULL while the secret is only asked for. A secret goes
-- with its agent.
CREATE TABLE agent_secrets (
    agent_id     TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    value        BLOB,
    -- While asked for and not set: why the agent asked, the shell variable
    -- it asked to get it as (or NULL), and when.
    reason       TEXT,
    env          TEXT,
    requested_at TEXT,
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (agent_id, name)
) STRICT;
