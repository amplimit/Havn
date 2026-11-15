-- Several conversations per chat (issue #126). A chat is one
-- conversation's history: its id is the `conversations.channel_id` of its
-- turns. `scope` is where its replies go (the dashboard session of a user,
-- or a channel sender), and `active_chats` names the chat of each scope
-- that new messages without a chosen chat continue.
CREATE TABLE chats (
    id          TEXT NOT NULL PRIMARY KEY,
    scope       TEXT NOT NULL,
    title       TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_chats_scope_updated ON chats(scope, updated_at);

CREATE TABLE active_chats (
    scope    TEXT NOT NULL PRIMARY KEY,
    chat_id  TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE
) STRICT;

-- Existing history becomes the first chat of its scope, under the same id,
-- titled after its first user message. Heartbeat and scheduled runs keep
-- their own history outside the chat list.
INSERT INTO chats (id, scope, title, created_at, updated_at)
SELECT c.channel_id,
       c.channel_id,
       -- The first line of the first user message, as `title_from` takes it.
       COALESCE((
           SELECT substr(trim(substr(t.body, 1, instr(t.body || char(10), char(10)) - 1)), 1, 80)
           FROM (
               SELECT ltrim(replace(f.content, char(13), ''), ' ' || char(9) || char(10)) AS body
               FROM conversations f
               WHERE f.channel_id = c.channel_id AND f.role = 'user'
               ORDER BY f.created_at, f.rowid
               LIMIT 1
           ) t
       ), ''),
       MIN(c.created_at),
       MAX(c.created_at)
FROM conversations c
WHERE c.channel_id <> 'agent:heartbeat' AND c.channel_id NOT LIKE 'cron:%'
GROUP BY c.channel_id;

INSERT INTO active_chats (scope, chat_id) SELECT scope, id FROM chats;
