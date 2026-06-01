-- havn — seed the two built-in system-wide roles (spec §6.4).
--
-- These are the policies the gateway falls back to when:
--   * a user belongs to a team and the team has no per-team admin/member
--     override yet (rare; teams created via the API auto-provision their
--     own role rows so this only fires for manually-poked teams), or
--   * a single-user-mode operator wants to inspect "what is the default
--     admin policy" without standing up a team first.
--
-- Both rows have `team_id IS NULL` to mark them as system-wide. They
-- are NOT deletable via the team-roles API (which scopes deletes by
-- team_id). The CLI `havn role delete` refuses on team_id IS NULL.
--
-- Idempotent: the WHERE NOT EXISTS guards re-runs (the migrator only
-- runs each file once, but seeded data is best protected anyway).
--
-- Stable IDs: UUID v7 with a deterministic-looking prefix (the leading
-- 8 hex digits encode "system" / "syssm" so an operator scanning the
-- DB can tell at a glance these are the seeds vs. user-created roles).
-- The remaining bytes are a constant nonce because UUID v7's timestamp
-- field is unimportant here — these rows aren't time-sortable in any
-- meaningful sense.

INSERT INTO roles (id, team_id, name, policy)
SELECT
    '01900000-0000-7000-8000-000000000001',
    NULL,
    'admin',
    -- Wide policy: every capability flag on, generous quotas. Operators
    -- editing this should clamp `max_agents` per their VPS budget.
    json('{
      "max_agents": 50,
      "allowed_models": ["*"],
      "resource_limits": { "memory_mb": 1024, "cpu_cores": 2.0, "pids_max": 128 },
      "budget": { "max_tokens_per_day": 0, "on_exhaust": "warn_and_pause" },
      "permissions": {
        "can_install_skills": true,
        "can_access_network": true,
        "can_use_shell": true,
        "can_view_own_logs": true,
        "can_export_memory": true,
        "can_bind_channels": null,
        "can_spawn_subagents": true,
        "can_schedule_cron": true
      },
      "network_policy": { "egress_allowed": true, "allowed_domains": ["*"], "blocked_domains": [] },
      "context_toolsets": {},
      "admin_visibility": {
        "can_view_agent_status": true,
        "can_view_agent_config": true,
        "can_view_conversations": false,
        "can_view_audit_log": true
      }
    }')
WHERE NOT EXISTS (
    SELECT 1 FROM roles WHERE team_id IS NULL AND name = 'admin'
);

INSERT INTO roles (id, team_id, name, policy)
SELECT
    '01900000-0000-7000-8000-000000000002',
    NULL,
    'member',
    -- Narrower policy: shell off, fewer agents, can't view team audit log.
    -- Conservative defaults; admins explicitly broaden via the role
    -- editor as needed.
    json('{
      "max_agents": 5,
      "allowed_models": ["*"],
      "resource_limits": { "memory_mb": 512, "cpu_cores": 1.0, "pids_max": 64 },
      "budget": { "max_tokens_per_day": 1000000, "on_exhaust": "warn_and_pause" },
      "permissions": {
        "can_install_skills": true,
        "can_access_network": true,
        "can_use_shell": false,
        "can_view_own_logs": true,
        "can_export_memory": true,
        "can_bind_channels": null,
        "can_spawn_subagents": false,
        "can_schedule_cron": true
      },
      "network_policy": { "egress_allowed": true, "allowed_domains": ["*"], "blocked_domains": [] },
      "context_toolsets": {
        "cron": { "disabled": ["shell"] }
      },
      "admin_visibility": {
        "can_view_agent_status": true,
        "can_view_agent_config": false,
        "can_view_conversations": false,
        "can_view_audit_log": false
      }
    }')
WHERE NOT EXISTS (
    SELECT 1 FROM roles WHERE team_id IS NULL AND name = 'member'
);
