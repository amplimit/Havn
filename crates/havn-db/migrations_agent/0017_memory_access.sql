-- Conversations not the owner's where memory search is always allowed
-- (issue #249), with the owner's rule for what not to say there.
CREATE TABLE memory_access (
    scope TEXT NOT NULL PRIMARY KEY,
    rule TEXT NOT NULL DEFAULT '',
    allowed_at TEXT NOT NULL
) STRICT;
