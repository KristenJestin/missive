-- What tells each message of a move from another (the copy's `message_id`, `size` and
-- `internal_date`, as `half_move` keeps them), taken when the move is recorded: the message is in
-- the copy then, under the UID the action names. Once the folder is renumbered, or the message
-- left the copy, the action can no longer take it from there, and a half move would have nothing
-- to be found again by. Kept for a move only, and gone with its action.
CREATE TABLE action_fingerprint (
    action INTEGER NOT NULL REFERENCES action (id) ON DELETE CASCADE,
    uid INTEGER NOT NULL,
    message_id TEXT,
    size INTEGER NOT NULL,
    internal_date INTEGER,
    PRIMARY KEY (action, uid)
) STRICT;
