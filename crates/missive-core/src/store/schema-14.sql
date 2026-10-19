-- When a copy waiting to be appended to Sent may be tried again, in milliseconds since the epoch:
-- a try that is counted (an `APPEND` started, or a look in Sent that failed) pushes it forward, so
-- that the limit of five tries means minutes and not the passes of a few seconds. 0: at once.
ALTER TABLE outbox ADD COLUMN copy_next_at INTEGER NOT NULL DEFAULT 0;
