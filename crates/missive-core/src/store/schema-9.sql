-- When a queued message may be tried again, in milliseconds since the epoch: each message waits
-- for its own time after a try that failed, so that one the server keeps refusing for now holds
-- back none of the others. 0: at once.
ALTER TABLE outbox ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0;
-- `copy_attempts` now counts the appends to Sent started, not those that failed: an append a
-- killed daemon left without its answer counts too, so that the next try looks in Sent first.
