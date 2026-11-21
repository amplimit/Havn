-- The chat of a web chat scope that keeps heartbeat results (issue #226),
-- under an id of its own: made again, under a new id, when deleted.
CREATE TABLE heartbeat_chats (
    scope   TEXT NOT NULL PRIMARY KEY,
    chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE
) STRICT;
