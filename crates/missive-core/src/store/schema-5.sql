-- The folders the last complete pass left as they were (the server refused them, or answered
-- something no sync can use), a JSON array of `{"folder", "kind"}` in the order of the pass: the
-- folder by its role, else its name, and the kind of error, never what the server said. Written
-- by the daemon when a pass ends, so that `status` can tell it from the local copy alone.
ALTER TABLE account ADD COLUMN skipped TEXT NOT NULL DEFAULT '[]';
