-- The highest vote count this account has sent on each poll.
CREATE TABLE poll_votes (
    author TEXT NOT NULL,
    sent_at_ms INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (author, sent_at_ms)
);
