-- A chat's own model (`/model` in a chat, issue #174); NULL for the
-- agent's.
ALTER TABLE chats ADD COLUMN model TEXT;
