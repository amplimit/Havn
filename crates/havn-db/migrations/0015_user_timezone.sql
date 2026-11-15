-- Each user's IANA time zone (issue #103); NULL means the gateway host's.
-- Numbered 0015: 0013 and 0014 are taken by web_fetch on the 0.2 line.
ALTER TABLE users ADD COLUMN timezone TEXT;
