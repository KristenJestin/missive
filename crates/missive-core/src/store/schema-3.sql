-- Whether a message has an attachment (a part `mail-parser` counts as one), decided from the raw
-- message when it is written, and, for the messages a copy already holds, when the daemon first
-- opens it. A list shows it for every row it prints: reading it from the raw messages at that
-- time would parse as many messages as the list has rows.
ALTER TABLE message ADD COLUMN has_attachments INTEGER NOT NULL DEFAULT 0;
