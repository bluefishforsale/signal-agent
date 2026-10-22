CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    author TEXT NOT NULL,
    sent_at_ms INTEGER NOT NULL,
    conversation TEXT NOT NULL,
    sender TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    received_at_ms INTEGER NOT NULL,
    body TEXT,
    attachments TEXT NOT NULL,
    quote_author TEXT,
    quote_sent_at_ms INTEGER,
    edited INTEGER NOT NULL,
    deleted INTEGER NOT NULL,
    read INTEGER NOT NULL DEFAULT 0,
    UNIQUE (author, sent_at_ms)
);

CREATE INDEX messages_by_conversation ON messages (conversation, sent_at_ms);

CREATE INDEX messages_unread ON messages (sent_at_ms) WHERE from_me = 0 AND read = 0;

CREATE TABLE titles (
    conversation TEXT PRIMARY KEY,
    title TEXT NOT NULL
);

-- External-content FTS over message bodies; the triggers keep it in step with
-- inserts, edits and row deletes.
CREATE VIRTUAL TABLE messages_fts USING fts5(
    body,
    content = 'messages',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;

CREATE TRIGGER messages_fts_update AFTER UPDATE OF body ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO messages_fts (rowid, body) VALUES (new.id, new.body);
END;
