-- `can_fetch_web` (v0.2.0) is on by default. A role policy or agent
-- override written before it that turns web search or network access off
-- turns web_fetch off too, so an upgrade does not open the web to agents
-- an admin kept away from it.
UPDATE roles
SET policy = json_set(policy, '$.permissions.can_fetch_web', json('false'))
WHERE json_valid(policy)
  AND json_type(policy, '$.permissions') = 'object'
  AND json_type(policy, '$.permissions.can_fetch_web') IS NULL
  AND (json_type(policy, '$.permissions.can_search_web') = 'false'
       OR json_type(policy, '$.permissions.can_access_network') = 'false');

UPDATE agents
SET config = json_set(config, '$.policy.permissions.can_fetch_web', json('false'))
WHERE json_valid(config)
  AND json_type(config, '$.policy.permissions') = 'object'
  AND json_type(config, '$.policy.permissions.can_fetch_web') IS NULL
  AND (json_type(config, '$.policy.permissions.can_search_web') = 'false'
       OR json_type(config, '$.policy.permissions.can_access_network') = 'false');
