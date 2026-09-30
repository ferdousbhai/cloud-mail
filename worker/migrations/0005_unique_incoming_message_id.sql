-- One stored copy per incoming Message-ID, even when copies are delivered concurrently
-- (Email Routing runs the worker once per recipient mailbox).
CREATE UNIQUE INDEX messages_incoming_message_id ON messages (message_id) WHERE outgoing = 0 AND message_id IS NOT NULL;
