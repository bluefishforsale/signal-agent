-- The highest vote count this account has sent on each poll.
CREATE TABLE poll_votes (
    author TEXT NOT NULL,
    sent_at_ms INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (author, sent_at_ms)
);

-- NOTE: AUTOINCREMENT, so a cancelled or finished job's id is never handed to
-- a new job; otherwise a stale cancel_scheduled could remove someone's next send.
CREATE TABLE scheduled_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    recipient TEXT NOT NULL,
    body TEXT NOT NULL,
    send_at_ms INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'sending', 'failed')),
    error TEXT
);

CREATE INDEX scheduled_jobs_by_due ON scheduled_jobs (state, send_at_ms);

-- One row per attachment, kept in step with messages by the triggers below, so
-- paging attachments walks an index instead of every message's JSON.
CREATE TABLE attachments (
    message INTEGER NOT NULL,
    position INTEGER NOT NULL,
    sent_at_ms INTEGER NOT NULL,
    id TEXT NOT NULL,
    attachment TEXT NOT NULL,
    PRIMARY KEY (message, position)
);

CREATE INDEX attachments_newest_first ON attachments (sent_at_ms DESC, message, position);

CREATE INDEX attachments_by_id ON attachments (id, message);

INSERT INTO attachments (message, position, sent_at_ms, id, attachment)
SELECT m.id, a.key, m.sent_at_ms, json_extract(a.value, '$.id'), a.value
FROM messages m, json_each(m.attachments) a;

CREATE TRIGGER attachments_insert AFTER INSERT ON messages BEGIN
    INSERT INTO attachments (message, position, sent_at_ms, id, attachment)
    SELECT new.id, a.key, new.sent_at_ms, json_extract(a.value, '$.id'), a.value
    FROM json_each(new.attachments) a;
END;

CREATE TRIGGER attachments_delete AFTER DELETE ON messages BEGIN
    DELETE FROM attachments WHERE message = old.id;
END;
