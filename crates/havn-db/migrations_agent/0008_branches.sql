-- Branches (issue #129): the turns of a chat form a tree. Each turn names
-- the turn it follows, and a chat names its head, the last turn of the
-- branch it continues; the model sees the path from the root to the head.
ALTER TABLE conversations ADD COLUMN parent_id TEXT;
ALTER TABLE chats ADD COLUMN head TEXT;

-- Existing turns become one branch per chat: each follows the turn before
-- it, in the order they were recorded.
UPDATE conversations
SET parent_id = prev.parent
FROM (
    SELECT id,
           LAG(id) OVER (PARTITION BY channel_id ORDER BY created_at, rowid) AS parent
    FROM conversations
    WHERE channel_id IN (SELECT id FROM chats)
) AS prev
WHERE conversations.id = prev.id;

UPDATE chats
SET head = (
    SELECT c.id
    FROM conversations c
    WHERE c.channel_id = chats.id
    ORDER BY c.created_at DESC, c.rowid DESC
    LIMIT 1
);

CREATE INDEX idx_conv_parent ON conversations(parent_id);
