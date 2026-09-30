-- One stored copy per incoming Message-ID, even when copies are delivered concurrently
-- (Email Routing runs the worker once per recipient mailbox).
-- Earlier versions could store concurrent copies twice; keep the first and let the later ones
-- stop claiming the id, so the index can be created on any existing database.
UPDATE messages SET message_id = NULL
WHERE outgoing = 0 AND message_id IS NOT NULL
  AND rowid NOT IN (SELECT MIN(rowid) FROM messages WHERE outgoing = 0 AND message_id IS NOT NULL GROUP BY message_id);
CREATE UNIQUE INDEX messages_incoming_message_id ON messages (message_id) WHERE outgoing = 0 AND message_id IS NOT NULL;
