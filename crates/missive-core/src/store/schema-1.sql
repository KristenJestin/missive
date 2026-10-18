-- An account of the configuration, by its identifier. `key` is the number logs name it by,
-- since the identifier is often an address.
CREATE TABLE account (
    key INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    -- no-login, auth-refused, certificate-refused, unreachable, syncing or ready.
    state TEXT NOT NULL,
    -- When the last complete sync ended, in seconds since the epoch.
    last_sync INTEGER
) STRICT;

-- A folder as the server lists it.
CREATE TABLE folder (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES account (key) ON DELETE CASCADE,
    -- The server's name for it, as IMAP spells it (modified UTF-7).
    name TEXT NOT NULL,
    -- inbox, sent, drafts, trash, junk or archive; NULL for a folder of the user's own.
    role TEXT,
    -- The UIDVALIDITY its messages' UIDs belong to; NULL until it is first examined.
    uidvalidity INTEGER,
    -- With CONDSTORE, the HIGHESTMODSEQ the flags in the copy are up to date with.
    highest_modseq INTEGER,
    UNIQUE (account, name)
) STRICT;

-- A message of a folder: the raw message as the server sent it, with what is read from it.
CREATE TABLE message (
    id INTEGER PRIMARY KEY,
    folder INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
    uid INTEGER NOT NULL,
    -- The IMAP flags, sorted and separated by spaces: "\Flagged \Seen $Label1".
    flags TEXT NOT NULL,
    -- The server's INTERNALDATE, in seconds since the epoch.
    internal_date INTEGER,
    -- The size the server gives (RFC822.SIZE), in bytes.
    size INTEGER NOT NULL,
    -- From the header, NULL when it is missing or unreadable. `date` is in seconds since the
    -- epoch; `sender`, `recipients` (To) and `cc` are JSON arrays of {"name", "address"}.
    date INTEGER,
    subject TEXT,
    sender TEXT NOT NULL,
    recipients TEXT NOT NULL,
    cc TEXT NOT NULL,
    message_id TEXT,
    raw BLOB NOT NULL,
    UNIQUE (folder, uid)
) STRICT;
