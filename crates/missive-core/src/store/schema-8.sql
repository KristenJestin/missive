-- The message (its identifier) a message of the outbox answers: once the server took the reply,
-- the original gets the \Answered flag, through the queue of actions, in the same transaction
-- that records the reply as sent. A reply only saved as a draft names nothing here.
ALTER TABLE outbox ADD COLUMN answers TEXT;
