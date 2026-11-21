-- Senders an `allowlist` account turned away (issue #231), for the
-- owner to see and admit: when they first and last wrote, how many
-- times, the start of what they last said, and when the owner was last
-- told about them.
CREATE TABLE channel_refusals (
    channel     TEXT NOT NULL,
    account_id  TEXT NOT NULL,
    sender_id   TEXT NOT NULL,
    first_at    TEXT NOT NULL,
    last_at     TEXT NOT NULL,
    count       INTEGER NOT NULL DEFAULT 1,
    said        TEXT NOT NULL DEFAULT '',
    notified_at TEXT,
    PRIMARY KEY (channel, account_id, sender_id)
) STRICT;

CREATE INDEX idx_channel_refusals_recent ON channel_refusals(channel, account_id, last_at);
