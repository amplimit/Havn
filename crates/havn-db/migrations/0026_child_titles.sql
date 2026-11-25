-- What a group kept as part of another is called, as the adapter last
-- named it (issue #321): a server's channel keeps its name once the
-- server is let in and its refusal, which held it, is forgotten.
ALTER TABLE channel_group_parents ADD COLUMN child_title TEXT NOT NULL DEFAULT '';
