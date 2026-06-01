-- havn — per-agent workspace database schema.
--
-- One file per agent at `<workspace>/agent.db`. Holds conversations, curated
-- memory, and a skill index for FTS5 retrieval at context build time.
-- See spec §5.2 (Storage Layout) and §9.4 (Memory System).

-- 1. Conversations: every turn (user / assistant / tool result) keyed by channel_id.

CREATE TABLE conversations (
    id          TEXT NOT NULL PRIMARY KEY,
    channel_id  TEXT NOT NULL,
    role        TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system', 'tool')),
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX idx_conv_channel_time ON conversations(channel_id, created_at);

-- FTS5 mirror of conversations.content for `memory_search` (spec §9.2).
-- Uses `content='conversations'` so the FTS index references the source rows
-- without duplicating data. Auxiliary triggers keep it in sync.

CREATE VIRTUAL TABLE conversations_fts USING fts5(
    content,
    content='conversations',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER conversations_ai AFTER INSERT ON conversations BEGIN
    INSERT INTO conversations_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER conversations_ad AFTER DELETE ON conversations BEGIN
    INSERT INTO conversations_fts(conversations_fts, rowid, content) VALUES('delete', old.rowid, old.content);
END;

CREATE TRIGGER conversations_au AFTER UPDATE ON conversations BEGIN
    INSERT INTO conversations_fts(conversations_fts, rowid, content) VALUES('delete', old.rowid, old.content);
    INSERT INTO conversations_fts(rowid, content) VALUES (new.rowid, new.content);
END;

-- 2. Memory: agent-curated key/value entries. Distinct from `MEMORY.md`
-- (which lives on disk and is frozen into the system prompt at session start).
-- This table is the runtime side of the `memory_store` / `memory_search` tools
-- (spec §9.2, §9.4 layer 1).

CREATE TABLE memory (
    id         TEXT NOT NULL PRIMARY KEY,
    key        TEXT NOT NULL UNIQUE,
    value      TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE VIRTUAL TABLE memory_fts USING fts5(
    key, value,
    content='memory',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER memory_ai AFTER INSERT ON memory BEGIN
    INSERT INTO memory_fts(rowid, key, value) VALUES (new.rowid, new.key, new.value);
END;

CREATE TRIGGER memory_ad AFTER DELETE ON memory BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, key, value) VALUES('delete', old.rowid, old.key, old.value);
END;

CREATE TRIGGER memory_au AFTER UPDATE ON memory BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, key, value) VALUES('delete', old.rowid, old.key, old.value);
    INSERT INTO memory_fts(rowid, key, value) VALUES (new.rowid, new.key, new.value);
END;

-- 3. Skills index: per-agent retrieval shadow of installed skills (spec §9.3).
-- The authoritative skill metadata lives in the gateway DB; this is the
-- per-agent FTS5 view used at context build time.

CREATE TABLE skills_index (
    id           TEXT NOT NULL PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    description  TEXT NOT NULL,
    body         TEXT NOT NULL,
    installed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE VIRTUAL TABLE skills_fts USING fts5(
    name, description, body,
    content='skills_index',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER skills_ai AFTER INSERT ON skills_index BEGIN
    INSERT INTO skills_fts(rowid, name, description, body)
    VALUES (new.rowid, new.name, new.description, new.body);
END;

CREATE TRIGGER skills_ad AFTER DELETE ON skills_index BEGIN
    INSERT INTO skills_fts(skills_fts, rowid, name, description, body)
    VALUES('delete', old.rowid, old.name, old.description, old.body);
END;

CREATE TRIGGER skills_au AFTER UPDATE ON skills_index BEGIN
    INSERT INTO skills_fts(skills_fts, rowid, name, description, body)
    VALUES('delete', old.rowid, old.name, old.description, old.body);
    INSERT INTO skills_fts(rowid, name, description, body)
    VALUES (new.rowid, new.name, new.description, new.body);
END;
