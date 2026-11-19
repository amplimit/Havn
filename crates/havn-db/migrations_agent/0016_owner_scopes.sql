-- The conversations the agent's owner writes from (issue #218): the web
-- chat, and chat-app accounts linked to them. The owner's search covers
-- the chats of all of them.
CREATE TABLE owner_scopes (
    scope TEXT NOT NULL PRIMARY KEY
) STRICT;

-- The web chat's conversations are the owner's: their scopes are the
-- dashboard's session ids, without the `:` and `|` of other scopes.
INSERT OR IGNORE INTO owner_scopes (scope)
    SELECT DISTINCT scope FROM chats WHERE scope NOT LIKE '%:%' AND scope NOT LIKE '%|%';
