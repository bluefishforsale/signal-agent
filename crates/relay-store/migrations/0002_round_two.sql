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
