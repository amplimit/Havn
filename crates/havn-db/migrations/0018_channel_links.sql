-- Pairing codes made on the web belong to the Havn user who made them
-- (issue #182): redeeming one links the sender to that user.
ALTER TABLE channel_invites ADD COLUMN user_id TEXT REFERENCES users(id) ON DELETE CASCADE;

-- Which Havn user a channel sender is. A sender is one user's; a user has
-- one sender per channel (linking again replaces it).
CREATE TABLE channel_links (
    channel   TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    linked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (channel, sender_id),
    UNIQUE (channel, user_id)
) STRICT;
