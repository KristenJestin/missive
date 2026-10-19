-- The message (its identifier) a message of the outbox forwards: once the server took the forward,
-- the original gets the $Forwarded keyword, through the queue of actions, as a reply gets
-- \Answered with `answers`. A forward only saved as a draft names nothing here.
ALTER TABLE outbox ADD COLUMN forwards TEXT;
