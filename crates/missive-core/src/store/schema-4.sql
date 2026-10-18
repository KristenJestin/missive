-- An action the user asked for on messages of one folder, kept until the server applied it or
-- refused it for good: the queue that lets actions be asked offline, and survive a daemon that
-- is killed. A request on messages of several folders is one row per folder. Rows are applied in
-- the order of their `id`. The row is written in the same transaction as the change it makes to
-- the copy at once (a flag, a message shown in its new folder, a message gone).
CREATE TABLE action (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES account (key) ON DELETE CASCADE,
    -- The folder the server holds the messages in.
    folder INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
    -- The UIDVALIDITY the UIDs belong to: once the folder has another, they name other messages.
    uidvalidity INTEGER NOT NULL,
    -- The messages' UIDs, a JSON array in ascending order.
    uids TEXT NOT NULL,
    -- read, unread, flagged, unflagged, move, delete (a move to the trash) or
    -- delete-permanently.
    kind TEXT NOT NULL,
    -- Where move and delete take the messages.
    target INTEGER REFERENCES folder (id) ON DELETE CASCADE,
    -- A move without MOVE goes through UID COPY: pending, copying (the COPY may have been sent,
    -- its answer was not read) or copied (only the removal from `folder` is left).
    state TEXT NOT NULL DEFAULT 'pending',
    -- The target's UIDNEXT just before the COPY: a copy made by it has a UID from there on.
    target_uidnext INTEGER,
    -- Once copied: the UIDs in the target, a JSON array in the order of `uids` (NULL for a
    -- message whose new UID is not known).
    copied TEXT,
    -- How many times the server answered NO to it, and the last kind of error.
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
) STRICT;
CREATE INDEX action_folder ON action (folder);

-- Set while a move or a delete waits for the server: the message is shown in this folder, while
-- the row stays where the server still has it (its `folder` and `uid`).
ALTER TABLE message ADD COLUMN moving_to INTEGER REFERENCES folder (id) ON DELETE SET NULL;
CREATE INDEX message_moving ON message (moving_to) WHERE moving_to IS NOT NULL;
