-- What a group turned away is called (issue #293), so that it is named,
-- not shown by its id, when it is let in.
ALTER TABLE channel_refusals ADD COLUMN title TEXT NOT NULL DEFAULT '';
