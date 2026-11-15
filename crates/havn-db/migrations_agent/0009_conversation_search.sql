-- Past-chat search (issue #105). The conversation index splits text on
-- letters and spaces, so a Chinese or Japanese sentence was one token and
-- a word inside it never matched. Trigrams match any substring of three
-- characters or more; shorter terms are matched with LIKE.
DROP TRIGGER conversations_ai;
DROP TRIGGER conversations_ad;
DROP TRIGGER conversations_au;
DROP TABLE conversations_fts;

CREATE VIRTUAL TABLE conversations_fts USING fts5(
    content,
    content='conversations',
    content_rowid='rowid',
    tokenize='trigram'
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

INSERT INTO conversations_fts(conversations_fts) VALUES('rebuild');
