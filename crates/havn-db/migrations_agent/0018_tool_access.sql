-- What the owner always allows in a conversation not theirs (issue #255):
-- a tool's results (`input` empty), or one exact call of a tool that runs
-- or changes things (`input` its whole input), with the owner's rule for
-- what not to say there. Memory search's permissions (issue #249) are its
-- first rows.
CREATE TABLE tool_access (
    scope TEXT NOT NULL,
    tool TEXT NOT NULL,
    input TEXT NOT NULL DEFAULT '',
    rule TEXT NOT NULL DEFAULT '',
    allowed_at TEXT NOT NULL,
    PRIMARY KEY (scope, tool, input)
) STRICT;

INSERT INTO tool_access (scope, tool, input, rule, allowed_at)
SELECT scope, 'memory_search', '', rule, allowed_at FROM memory_access;

DROP TABLE memory_access;
