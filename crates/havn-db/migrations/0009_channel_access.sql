-- Migration 0009 — invite-code pairing for channel DMs (issue #25).
--
-- An operator creates a one-time invite for one (channel, account) pair
-- and hands the code out of band. An unauthorized sender who messages
-- the bot that code is added to `channel_allowlist`; from then on the
-- account's `dm_policy = "allowlist"` admits them alongside the TOML
-- `allow_from` entries.
--
-- Only the SHA-256 of the canonical code is stored: the code itself is
-- printed once by `havn invite create` and never persisted. Timestamps
-- are RFC3339 UTC with millisecond precision, the same width as the
-- strftime defaults, so they compare lexicographically.

CREATE TABLE channel_invites (
    id             TEXT NOT NULL PRIMARY KEY,          -- UUID v7
    code_hash      TEXT NOT NULL UNIQUE,               -- hex SHA-256 of the canonical code
    channel        TEXT NOT NULL,
    account_id     TEXT NOT NULL,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at     TEXT NOT NULL,
    used_by_sender TEXT,
    used_at        TEXT,
    revoked_at     TEXT,
    CHECK ((used_by_sender IS NULL) = (used_at IS NULL))
) STRICT;

CREATE INDEX idx_channel_invites_account ON channel_invites(channel, account_id, created_at);

-- `invite_id` records which invite admitted the sender (the issue's
-- `invite_code`, by reference since the code is not stored).
CREATE TABLE channel_allowlist (
    channel       TEXT NOT NULL,
    account_id    TEXT NOT NULL,
    sender_id     TEXT NOT NULL,
    registered_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    invite_id     TEXT REFERENCES channel_invites(id) ON DELETE SET NULL,
    PRIMARY KEY (channel, account_id, sender_id)
) STRICT;
