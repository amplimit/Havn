-- Branches (issue #129): the turns of a chat form a tree. Each turn names
-- the turn it follows, and a chat names its head, the last turn of the
-- branch it continues; the model sees the path from the root to the head.
ALTER TABLE conversations ADD COLUMN parent_id TEXT;
ALTER TABLE chats ADD COLUMN head TEXT;

-- Existing turns become one branch per chat: each follows the turn before
-- it.
UPDATE conversations
SET parent_id = (
    SELECT p.id
    FROM conversations p
    WHERE p.channel_id = conversations.channel_id
      AND (p.created_at < conversations.created_at
           OR (p.created_at = conversations.created_at AND p.rowid < conversations.rowid))
    ORDER BY p.created_at DESC, p.rowid DESC
    LIMIT 1
)
WHERE channel_id IN (SELECT id FROM chats);

UPDATE chats
SET head = (
    SELECT c.id
    FROM conversations c
    WHERE c.channel_id = chats.id
    ORDER BY c.created_at DESC, c.rowid DESC
    LIMIT 1
);

CREATE INDEX idx_conv_parent ON conversations(parent_id);
