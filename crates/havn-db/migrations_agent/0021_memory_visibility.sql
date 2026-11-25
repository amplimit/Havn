-- Who a fact is put in front of (issue #309): `private`, the owner's own
-- private chats only, as every fact was until now; or `public`, every
-- chat, other people's and groups' included. Only the owner makes a fact
-- public, and public facts do not age out.
ALTER TABLE memory ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private'
    CHECK (visibility IN ('private', 'public'));

-- The agent suggests making a private fact public; the owner decides on
-- the dashboard, which clears it either way.
ALTER TABLE memory ADD COLUMN suggested_public INTEGER NOT NULL DEFAULT 0
    CHECK (suggested_public IN (0, 1));

CREATE INDEX idx_memory_public ON memory(visibility)
    WHERE archived_at IS NULL AND visibility = 'public';

-- Bootstrap files still to fold into memory, once each: `USER.md` becomes
-- a public fact on the agent's next start, and is then kept as a backup
-- and no longer read. An agent made later has none to fold, and a file
-- put there afterwards is not read either.
CREATE TABLE bootstrap_imports (
    file TEXT NOT NULL PRIMARY KEY
) STRICT;

INSERT INTO bootstrap_imports (file) VALUES ('USER.md');
