-- A reply that tells of a failure instead of answering (issue #217): kept,
-- so a turn's steps and its branch stay, and read back as no answer.
ALTER TABLE conversations ADD COLUMN failed INTEGER NOT NULL DEFAULT 0;
