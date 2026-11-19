-- The conversations the agent's owner writes from (issue #218): the web
-- chat, and chat-app accounts linked to them. The owner's search covers
-- the chats of all of them.
CREATE TABLE owner_scopes (
    scope TEXT NOT NULL PRIMARY KEY
) STRICT;
