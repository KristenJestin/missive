-- The UIDs of the draft's older versions it marked \Deleted itself on the server, a JSON array,
-- while the flag may still be set, as `action.marked_deleted`: a connection lost before the
-- `UID EXPUNGE` or before the flag was taken back leaves it there, and the next try still knows
-- it is Missive's own. What is left of it when the row leaves goes to `deleted_left`.
ALTER TABLE draft ADD COLUMN marked_deleted TEXT;

-- A move refused while its `UID COPY` had no answer yet (its state was `copying`): the server may
-- or may not have copied the messages, so they may be in both folders. `target_uid` is NULL.
ALTER TABLE half_move ADD COLUMN maybe INTEGER NOT NULL DEFAULT 0;
