-- Full-text search over a message's sender, recipients (To and Cc), subject and body text. The
-- table holds the index only, not the text: the text is read from the raw message when the
-- message is written, so that it is not stored twice. A row's `rowid` is the `id` of its message
-- in `message`, which the writer fills and the trigger below empties.
--
-- `unicode61` with `remove_diacritics 2` makes case and accents not matter (`Élodie` finds
-- `elodie`); it cuts words at every character that is not a letter or a digit, so an address
-- becomes its parts (`alice`, `example`, `org`).
CREATE VIRTUAL TABLE message_search USING fts5 (
    sender,
    recipients,
    subject,
    body,
    content = '',
    contentless_delete = 1,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- A message leaves the index with its row, however it leaves (an expunge, a rebuilt folder, an
-- account or folder deleted).
CREATE TRIGGER message_search_delete AFTER DELETE ON message
BEGIN
    DELETE FROM message_search WHERE rowid = old.id;
END;

-- What `unread` counts, read from here alone: the flags of each folder's messages, without
-- reading the messages' rows.
CREATE INDEX message_flags ON message (folder, flags);
