-- The chat of a web chat scope that keeps what a webhook brought and what
-- the agent made of it (issue #236), one per webhook: made again, under a
-- new id, when deleted. Keyed by scope too, so a chat made in another scope
-- does not take the place of one that stays.
CREATE TABLE webhook_chats (
    webhook_id TEXT NOT NULL,
    scope      TEXT NOT NULL,
    chat_id    TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    PRIMARY KEY (webhook_id, scope)
) STRICT;

-- Every chat that ever kept a webhook's calls stays known as one.
CREATE INDEX idx_webhook_chats_chat ON webhook_chats(chat_id);
