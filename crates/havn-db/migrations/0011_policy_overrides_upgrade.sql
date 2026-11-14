-- Gateways before v0.2.0 read a per-agent `config.policy` as a whole
-- policy: keys it left out took the defaults. Newer gateways lay it over
-- the owner's policy instead. A row here marks a database that may hold
-- such partial overrides: the gateway rewrites each one once at start as
-- the full policy it stood for, and deletes the row.
CREATE TABLE policy_overrides_upgrade (
    id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1)
) STRICT;

INSERT INTO policy_overrides_upgrade (id)
SELECT 1
WHERE EXISTS (
    SELECT 1 FROM agents
    WHERE json_valid(config) AND json_type(config, '$.policy') = 'object'
);
