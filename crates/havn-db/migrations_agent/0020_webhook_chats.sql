-- The chat of a web chat scope that keeps what a webhook brought and what
-- the agent made of it (issue #236), one per webhook: made again, under a
-- new id, when deleted.
CREATE TABLE webhook_chats (
    webhook_id TEXT NOT NULL PRIMARY KEY,
    scope      TEXT NOT NULL,
    chat_id    TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE
) STRICT;
