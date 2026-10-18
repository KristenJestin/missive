-- A draft whose latest version is still to be written to the account's Drafts folder, or which
-- is to be removed from it (it was sent): kept until the server holds that version, offline
-- included, and across a daemon that is killed. One row per draft, named by its Message-ID,
-- which every version of the draft keeps. While the row exists, it is the draft: the versions
-- the copy holds of it in the Drafts folder are older ones, and are not listed.
CREATE TABLE draft (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES account (key) ON DELETE CASCADE,
    -- Without its angle brackets.
    message_id TEXT NOT NULL,
    -- The version to append, its Bcc header included; NULL when the draft is only to be removed
    -- from the server, its message having been sent.
    raw BLOB,
    -- What a list shows of it, read from `raw` when it is written, as for `message`.
    date INTEGER,
    subject TEXT,
    sender TEXT NOT NULL DEFAULT '[]',
    recipients TEXT NOT NULL DEFAULT '[]',
    has_attachments INTEGER NOT NULL DEFAULT 0,
    -- Versions of the draft on the server that have no Message-ID to be found by (a draft
    -- another client wrote without one): a JSON array of UIDs of the Drafts folder, under
    -- `replaces_uidvalidity`.
    replaces TEXT NOT NULL DEFAULT '[]',
    replaces_uidvalidity INTEGER,
    -- Counts every change of the row: a version written to the server leaves the row only if
    -- no other version was saved meanwhile.
    version INTEGER NOT NULL DEFAULT 1,
    -- When the version was saved, in seconds since the epoch.
    saved_at INTEGER NOT NULL,
    -- How many times writing it failed, and the last kind of error.
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    UNIQUE (account, message_id)
) STRICT;

-- The draft (its Message-ID) a message of the outbox was sent from: once the server took the
-- message, the draft is removed from the Drafts folder.
ALTER TABLE outbox ADD COLUMN draft TEXT;
