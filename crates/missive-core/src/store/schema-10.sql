-- A message whose move the server refused after it had copied it to the target: it is in both
-- folders, and only the user removes one of them. One row per message, kept while both are
-- still in the copy where the row says (`missive status`, `half_moved`).
CREATE TABLE half_move (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES account (key) ON DELETE CASCADE,
    -- Where the server still has the message.
    folder INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
    uidvalidity INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    -- Where it was copied, and its UID there when the server told it (COPYUID).
    target INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
    target_uidvalidity INTEGER,
    target_uid INTEGER
) STRICT;
CREATE INDEX half_move_account ON half_move (account);
