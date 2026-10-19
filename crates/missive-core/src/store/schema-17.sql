-- What tells a half-moved message from another (the copy's `message_id`, `size` and
-- `internal_date`, which `UID COPY` keeps), as `find_copy` trusts a copy found again: once a folder
-- is renumbered, its UIDs name other messages, and the message is found again by these. NULL when
-- the copy did not have the message under the action's UIDVALIDITY.
ALTER TABLE half_move ADD COLUMN message_id TEXT;
ALTER TABLE half_move ADD COLUMN size INTEGER;
ALTER TABLE half_move ADD COLUMN internal_date INTEGER;
UPDATE half_move SET (message_id, size, internal_date) = (
    SELECT message.message_id, message.size, message.internal_date
      FROM message JOIN folder ON folder.id = message.folder
     WHERE message.folder = half_move.folder AND message.uid = half_move.uid
       AND folder.uidvalidity = half_move.uidvalidity);
