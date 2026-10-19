-- The drafts Missive wrote itself, by account and Message-ID, with the message each one answers
-- or forwards: the only drafts whose `X-Missive-Answers` and `X-Missive-Forwards` headers are
-- believed. Anyone who can write to the Drafts folder can put those headers in a draft; one that
-- Missive did not write is picked up and sent as it is, and marks nothing. A header is trusted
-- only when it says what was recorded here, so a version of the same draft written by another
-- client does not carry a link over. The row leaves with the draft, once its message was sent.
CREATE TABLE own_draft (
    account INTEGER NOT NULL REFERENCES account (key) ON DELETE CASCADE,
    -- Without its angle brackets.
    message_id TEXT NOT NULL,
    answers TEXT,
    forwards TEXT,
    PRIMARY KEY (account, message_id)
) STRICT, WITHOUT ROWID;
