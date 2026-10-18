-- A message to send, kept from the moment it was written until its server took it, offline
-- included, and across a daemon that is killed: the outbox. Rows are sent in the order of their
-- `id`. A message leaves it only after the server's 250 to its end of data, and once its copy is
-- in the Sent folder when Missive writes that copy.
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES account (key) ON DELETE CASCADE,
    -- queued: to send (again). claimed: a worker took it, and sent nothing of it yet; a row
    -- found so when the daemon starts is queued again. sending: its end of data is being written, so the server may take
    -- it; a row found so after a daemon was killed becomes uncertain. sent: the server took it,
    -- and its copy is still to be appended to Sent. failed: the server refused it for good (5xx).
    -- uncertain: the connection was lost after its end of data and before the server's answer;
    -- never sent again by itself, since a duplicate is worse than a question.
    state TEXT NOT NULL DEFAULT 'queued',
    -- The envelope: MAIL FROM, and RCPT TO as a JSON array (To, Cc and Bcc).
    sender TEXT NOT NULL,
    recipients TEXT NOT NULL,
    -- Without its angle brackets.
    message_id TEXT NOT NULL,
    -- The message as built and as sent, Bcc left out.
    raw BLOB NOT NULL,
    -- The `Bcc:` header put before `raw` in the copy appended to Sent; NULL without Bcc.
    bcc_header BLOB,
    -- Whether Missive appends the copy to Sent (the account does not say its server does).
    sent_copy INTEGER NOT NULL,
    -- When it was written, in seconds since the epoch.
    queued_at INTEGER NOT NULL,
    -- The tries that failed before its end of data (a 4xx, a lost connection, a refused login),
    -- and the last error, for people.
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    -- How many times appending its copy to Sent failed.
    copy_attempts INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX outbox_account ON outbox (account, state);
