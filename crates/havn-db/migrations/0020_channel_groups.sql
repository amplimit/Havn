-- Groups a channel account may be used in (issue #263), besides the
-- config's `allow_groups`: let in by the agent's owner (`owner`), or used
-- while the account's `group_policy` was `open` (`open`), which counts
-- only while it still is.
CREATE TABLE channel_groups (
    channel       TEXT NOT NULL,
    account_id    TEXT NOT NULL,
    group_id      TEXT NOT NULL,
    source        TEXT NOT NULL CHECK (source IN ('owner', 'open')),
    title         TEXT NOT NULL DEFAULT '',
    registered_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (channel, account_id, group_id)
) STRICT;

-- A group turned away is listed with the senders (issue #231), as a group.
ALTER TABLE channel_refusals ADD COLUMN is_group INTEGER NOT NULL DEFAULT 0;
