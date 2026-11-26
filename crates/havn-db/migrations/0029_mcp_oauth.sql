-- havn — OAuth of an agent's remote MCP servers (issue #234).
--
-- One row per MCP server of an agent its owner connected with OAuth.
-- `value` is age ciphertext made with the gateway's HAVN_AGE_KEY, as agent
-- secrets' `value` is: the authorization server's metadata, the client the
-- gateway registered or was given, and the tokens. `url` is the server URL
-- they were issued for; they are sent nowhere else. A row goes with its
-- agent.
CREATE TABLE mcp_oauth (
    agent_id    TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    server      TEXT NOT NULL,
    url         TEXT NOT NULL,
    value       BLOB NOT NULL,
    -- When the access token expires, when the provider said; it is
    -- refreshed before.
    expires_at  TEXT,
    -- Why the last refresh failed, until one does not.
    error       TEXT,
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (agent_id, server)
) STRICT;
