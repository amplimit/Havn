-- havn — v0.6 user table simplification (spec §1.6 / §1.7 / §5.1).
--
-- havn does no auth. Identity comes from the upstream reverse proxy
-- via the X-User-ID header; the User row is just an authorisation key.
-- The earlier `email` column was UNIQUE-constrained, which SQLite
-- won't let us drop with `ALTER TABLE … DROP COLUMN`. So we rebuild
-- the table per https://sqlite.org/lang_altertable.html#otheralter.
--
-- The table-rebuild dance:
-- 1. Create new schema under a temp name.
-- 2. Copy rows over, projecting only the columns we keep and renaming
--    `name` → `display_name` in flight.
-- 3. Drop the old table (which cascades through FK references —
--    sqlx's migrator runs each file in its own transaction with
--    foreign_keys ON, so any rows in dependent tables that point at a
--    soon-to-be-orphaned user would block; in practice the FKs all
--    cascade ON DELETE so this only fires if there are dangling
--    rows, which there shouldn't be).
-- 4. Rename the new table into place.

CREATE TABLE users_new (
    id            TEXT NOT NULL PRIMARY KEY,
    display_name  TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

INSERT INTO users_new (id, display_name, created_at)
    SELECT id, name, created_at FROM users;

-- Foreign keys from other tables (team_memberships.user_id, agents.owner_id,
-- credentials with scope=user, credential_usages.user_id, audit_log.user_id)
-- all reference `users(id)`. Dropping and recreating the table while keeping
-- the same primary-key values is the canonical SQLite approach; the FK
-- references resolve against the post-rename table without further work.
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;
