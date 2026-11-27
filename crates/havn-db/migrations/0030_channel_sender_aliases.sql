-- Ids a channel sender went by before on an account (issue #262), such as
-- Lark's per-app open_id before its union_id: noted when an inbound on the
-- account names one. What was kept under the old id stays there, and its
-- admission counts for the new one. `link_user` is the Havn user whose link
-- of the old id moved to the new one: only theirs are the old id's chats.
CREATE TABLE channel_sender_aliases (
    channel    TEXT NOT NULL,
    account_id TEXT NOT NULL,
    alias      TEXT NOT NULL,
    sender_id  TEXT NOT NULL,
    link_user  TEXT REFERENCES users(id) ON DELETE SET NULL,
    noted_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (channel, account_id, alias)
) STRICT;

CREATE INDEX idx_channel_sender_aliases_sender ON channel_sender_aliases(channel, account_id, sender_id);
