-- A chat's own thinking choice (issue #190): off, on, a level or a
-- budget; NULL for its model's default.
ALTER TABLE chats ADD COLUMN thinking TEXT;
