-- When a queued message may be tried again, in milliseconds since the epoch: each message waits
-- for its own time after a try that failed, so that one the server keeps refusing for now holds
-- back none of the others. 0: at once.
ALTER TABLE outbox ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0;
