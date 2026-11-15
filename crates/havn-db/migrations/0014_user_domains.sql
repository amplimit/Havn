-- A user's own allowed and blocked domains (issue #102), added to the
-- policy of every agent they own. Each column is a JSON array of domains.
CREATE TABLE user_domains (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    allowed TEXT NOT NULL DEFAULT '[]',
    blocked TEXT NOT NULL DEFAULT '[]'
) STRICT;
