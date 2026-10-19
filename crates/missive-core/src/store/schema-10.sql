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

-- The last number given to an action. A row's `id` alone could be given again once the last
-- action left the queue; with this, a number names one action for ever, so that `--wait` (and
-- any client) can match the `action` event to the request that recorded it.
CREATE TABLE action_sequence (last INTEGER NOT NULL) STRICT;
INSERT INTO action_sequence (last) SELECT coalesce(max(id), 0) FROM action;

-- What the account's IMAP server can do, recorded at each connection, so that an action it cannot
-- apply is refused when it is asked: whether it has MOVE (RFC 6851) and UIDPLUS (RFC 4315).
-- NULL until the first connection.
ALTER TABLE account ADD COLUMN can_move INTEGER;
ALTER TABLE account ADD COLUMN can_uidplus INTEGER;
