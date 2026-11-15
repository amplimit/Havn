-- Long chats (issue #105): what a chat said before the turns the model is
-- given, summarised. A summary covers its branch from the first turn
-- through `turn_id`; turns on other branches have their own.
CREATE TABLE chat_summaries (
    turn_id     TEXT NOT NULL PRIMARY KEY,
    chat        TEXT NOT NULL,
    summary     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_chat_summaries_chat ON chat_summaries(chat);
