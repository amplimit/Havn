-- Each agent's slot in the range of host uids and gids its sandbox runs
-- as under a root gateway. AUTOINCREMENT never hands a slot out twice,
-- even after its agent is deleted, so what a deleted agent left behind
-- (files in a shared read-write mount, say) never passes to a new one.
-- Numbered 0012 because 0011 is taken on the 0.2 line.
CREATE TABLE agent_host_ids (
    slot     INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL UNIQUE REFERENCES agents(id) ON DELETE CASCADE
) STRICT;
