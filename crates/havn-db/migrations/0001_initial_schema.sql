-- havn — initial schema, gateway database.
--
-- All tables STRICT (SQLite ≥ 3.37). IDs are UUID v7 stored as TEXT.
-- Timestamps are RFC 3339 strings (SQLite ``datetime('now', 'subsec')``).
-- Foreign keys are enforced at the connection level via ``PRAGMA foreign_keys = ON``.

CREATE TABLE users (
    id            TEXT    PRIMARY KEY NOT NULL,
    email         TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL,
    auth_provider TEXT    NOT NULL DEFAULT 'email',
    created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE TABLE teams (
    id         TEXT NOT NULL PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE TABLE roles (
    id         TEXT NOT NULL PRIMARY KEY,
    team_id    TEXT REFERENCES teams(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    policy     TEXT NOT NULL DEFAULT '{}',  -- JSON; validated at app layer
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_roles_team ON roles(team_id);

CREATE TABLE team_memberships (
    user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id   TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    role_id   TEXT NOT NULL REFERENCES roles(id) ON DELETE RESTRICT,
    joined_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (user_id, team_id)
) STRICT;

CREATE INDEX idx_memberships_team ON team_memberships(team_id);

CREATE TABLE agents (
    id         TEXT NOT NULL PRIMARY KEY,
    owner_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id    TEXT REFERENCES teams(id) ON DELETE SET NULL,
    name       TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'created'
        CHECK (status IN ('created', 'running', 'paused', 'stopped', 'error')),
    host       TEXT,
    pid        INTEGER,
    config     TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (owner_id, name)
) STRICT;

CREATE INDEX idx_agents_owner ON agents(owner_id);
CREATE INDEX idx_agents_team  ON agents(team_id);

CREATE TABLE channel_bindings (
    id              TEXT    NOT NULL PRIMARY KEY,
    agent_id        TEXT    NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    platform        TEXT    NOT NULL,
    platform_config BLOB    NOT NULL,            -- encrypted in Phase 3
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_bindings_agent ON channel_bindings(agent_id);

CREATE TABLE credentials (
    id          TEXT    NOT NULL PRIMARY KEY,
    scope       TEXT    NOT NULL CHECK (scope IN ('user', 'team')),
    scope_id    TEXT    NOT NULL,                -- FK to users.id or teams.id (validated at app layer)
    provider    TEXT    NOT NULL,
    api_key     BLOB    NOT NULL,                -- encrypted in Phase 3
    priority    INTEGER NOT NULL DEFAULT 0,
    limits      TEXT    NOT NULL DEFAULT '{}',
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_credentials_scope ON credentials(scope, scope_id, provider, priority DESC);

CREATE TABLE credential_usages (
    id             TEXT    NOT NULL PRIMARY KEY,
    credential_id  TEXT    NOT NULL REFERENCES credentials(id) ON DELETE CASCADE,
    user_id        TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Nullable: gateway-direct LLM calls (e.g. the test endpoint) have no agent.
    agent_id       TEXT    REFERENCES agents(id) ON DELETE CASCADE,
    provider       TEXT    NOT NULL,
    model          TEXT    NOT NULL,
    tokens_in      INTEGER NOT NULL DEFAULT 0,
    tokens_out     INTEGER NOT NULL DEFAULT 0,
    estimated_usd  REAL    NOT NULL DEFAULT 0.0,
    created_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_credusage_user_time       ON credential_usages(user_id, created_at);
CREATE INDEX idx_credusage_credential_time ON credential_usages(credential_id, created_at);
CREATE INDEX idx_credusage_agent_time      ON credential_usages(agent_id, created_at);

CREATE TABLE cron_jobs (
    id               TEXT    NOT NULL PRIMARY KEY,
    agent_id         TEXT    NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    name             TEXT    NOT NULL,
    prompt           TEXT    NOT NULL,
    schedule         TEXT    NOT NULL,
    next_run_at      TEXT    NOT NULL,
    wake_check       TEXT,
    context_from     TEXT    NOT NULL DEFAULT '[]',
    deliver          TEXT    NOT NULL DEFAULT 'local',
    origin           TEXT,
    enabled_toolsets TEXT    NOT NULL DEFAULT '[]',
    enabled          INTEGER NOT NULL DEFAULT 1,
    created_at       TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_cron_jobs_due ON cron_jobs(next_run_at) WHERE enabled = 1;
CREATE INDEX idx_cron_jobs_agent ON cron_jobs(agent_id);

CREATE TABLE cron_runs (
    id          TEXT    NOT NULL PRIMARY KEY,
    job_id      TEXT    NOT NULL REFERENCES cron_jobs(id) ON DELETE CASCADE,
    started_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at TEXT,
    output_path TEXT    NOT NULL,
    silent      INTEGER NOT NULL DEFAULT 0,
    error       TEXT
) STRICT;

CREATE INDEX idx_cronruns_job ON cron_runs(job_id, started_at);

CREATE TABLE skills (
    id            TEXT    NOT NULL PRIMARY KEY,
    agent_id      TEXT    NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    name          TEXT    NOT NULL,
    description   TEXT    NOT NULL,
    version       TEXT,
    source        TEXT    NOT NULL CHECK (source IN ('user_uploaded', 'agent_created', 'team_shared', 'bundled')),
    pinned        INTEGER NOT NULL DEFAULT 0,
    absorbed_into TEXT    REFERENCES skills(id) ON DELETE SET NULL,
    installed_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (agent_id, name)
) STRICT;

CREATE INDEX idx_skills_agent_source ON skills(agent_id, source);

CREATE TABLE skill_usages (
    id               TEXT    NOT NULL PRIMARY KEY,
    skill_id         TEXT    NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    agent_id         TEXT    NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    fired_at         TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    task_duration_ms INTEGER,
    outcome          TEXT    NOT NULL DEFAULT 'unknown'
        CHECK (outcome IN ('success', 'error', 'abandoned', 'unknown'))
) STRICT;

CREATE INDEX idx_skillusage_skill_time ON skill_usages(skill_id, fired_at);

CREATE TABLE curator_reports (
    id               TEXT NOT NULL PRIMARY KEY,
    agent_id         TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    ran_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    consolidations   TEXT NOT NULL DEFAULT '[]',
    prunings         TEXT NOT NULL DEFAULT '[]',
    full_report_path TEXT NOT NULL
) STRICT;

CREATE INDEX idx_curator_agent_time ON curator_reports(agent_id, ran_at);

CREATE TABLE audit_log (
    id         TEXT NOT NULL PRIMARY KEY,
    team_id    TEXT REFERENCES teams(id) ON DELETE SET NULL,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    agent_id   TEXT REFERENCES agents(id) ON DELETE SET NULL,
    action     TEXT NOT NULL,
    details    TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_audit_team_time  ON audit_log(team_id, created_at);
CREATE INDEX idx_audit_user_time  ON audit_log(user_id, created_at);
CREATE INDEX idx_audit_agent_time ON audit_log(agent_id, created_at);
