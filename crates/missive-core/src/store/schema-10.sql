-- How many times looking for a sent message's copy in the Sent folder failed (the connection was
-- lost, or the server refused the search): such a try says nothing of the copy, so it is not one of
-- `copy_attempts`, but it is bounded too, so that a server that never answers it does not keep the
-- copy for ever.
ALTER TABLE outbox ADD COLUMN copy_search_failures INTEGER NOT NULL DEFAULT 0;
