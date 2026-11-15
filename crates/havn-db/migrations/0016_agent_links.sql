-- Each agent's link in the agent subnet pool: the /30 and veth names
-- its sandbox network uses. Unique, so no two agents share addresses;
-- freed when the agent is deleted.
CREATE TABLE agent_links (
    slot     INTEGER PRIMARY KEY CHECK (slot BETWEEN 0 AND 16383),
    agent_id TEXT NOT NULL UNIQUE REFERENCES agents(id) ON DELETE CASCADE
) STRICT;
