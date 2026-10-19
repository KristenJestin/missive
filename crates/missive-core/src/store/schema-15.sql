-- Whether `copied` is what the server said (COPYUID), rather than what was found again in the
-- target after a `UID COPY` whose answer never came: only a UID the server gave for exactly that
-- message lets the actions asked after the move follow it there.
ALTER TABLE action ADD COLUMN copyuid INTEGER NOT NULL DEFAULT 0;
-- The UIDs of `uids` this action itself marked \Deleted on the server, a JSON array, while the
-- flag may still be set: a connection lost before its `UID EXPUNGE` or before the flag was taken
-- back leaves it there. A later try takes it back from these, and only these.
ALTER TABLE action ADD COLUMN marked_deleted TEXT;

-- What an action refused for good left marked \Deleted on the server, that Missive marked itself:
-- the flag is taken back at the next connection (`missive status`, `deleted_left`, until then).
CREATE TABLE deleted_left (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES account (key) ON DELETE CASCADE,
    folder INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
    -- Once the folder has another, the UIDs name other messages: the row is dropped.
    uidvalidity INTEGER NOT NULL,
    -- A JSON array in ascending order.
    uids TEXT NOT NULL
) STRICT;
CREATE INDEX deleted_left_account ON deleted_left (account);
