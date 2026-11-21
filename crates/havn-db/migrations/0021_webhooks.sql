-- Webhooks (issue #236): an address outside systems call to have an agent
-- act on what they send. `secret` is kept as given out, since a caller
-- that signs (GitHub's X-Hub-Signature-256) is checked with it.
CREATE TABLE webhooks (
    id             TEXT NOT NULL PRIMARY KEY,
    agent_id       TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    secret         TEXT NOT NULL,
    -- What the agent is to do with a call, written by the owner.
    instructions   TEXT NOT NULL DEFAULT '',
    -- Where results go: `web` (its chat only), `last` (also where the
    -- owner wrote last), or a channel's name.
    deliver        TEXT NOT NULL DEFAULT 'web',
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_called_at TEXT,
    calls          INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX idx_webhooks_agent ON webhooks(agent_id);
