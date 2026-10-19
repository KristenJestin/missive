-- The draft of another client (its Message-ID, else the identifier the request named) a message
-- of the outbox was sent from. That draft stays on the server as it is, so unlike `draft` this is
-- never removed: it only tells that a send asked again, after an answer was lost, is the same
-- send while that message is still to be sent.
ALTER TABLE outbox ADD COLUMN source TEXT;
