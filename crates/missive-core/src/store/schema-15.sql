-- Whether `copied` is what the server said (COPYUID), rather than what was found again in the
-- target after a `UID COPY` whose answer never came: only a UID the server gave for exactly that
-- message lets the actions asked after the move follow it there.
ALTER TABLE action ADD COLUMN copyuid INTEGER NOT NULL DEFAULT 0;
