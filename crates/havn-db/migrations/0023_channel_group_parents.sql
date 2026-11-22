-- What a group is part of (issue #285): a thread its channel and server,
-- a channel its server. A group may use a channel account while it, or
-- one of these, may; kept as the adapter names them, so the agent can
-- send to a thread later.
CREATE TABLE channel_group_parents (
    channel      TEXT NOT NULL,
    account_id   TEXT NOT NULL,
    child_id     TEXT NOT NULL,
    parent_id    TEXT NOT NULL,
    parent_title TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (channel, account_id, child_id, parent_id)
) STRICT;

CREATE INDEX idx_channel_group_parents_parent
    ON channel_group_parents (channel, account_id, parent_id);
