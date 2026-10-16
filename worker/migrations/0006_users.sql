-- Several people on one worker. The API_TOKEN secret is the admin ("owner"), who invites others; each
-- has their own token, mailboxes, threads, screening decisions and settings. Everything that existed
-- before belongs to the owner.
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  role TEXT NOT NULL CHECK (role IN ('admin', 'user')),
  -- SHA-256 (hex) of the user's API token and of their pending one-time invite code.
  token_hash TEXT UNIQUE,
  invite_hash TEXT UNIQUE,
  invite_expires_at INTEGER,
  created_at INTEGER NOT NULL,
  revoked_at INTEGER
);
INSERT INTO users (id, name, role, created_at) VALUES ('owner', 'owner', 'admin', 0);

ALTER TABLE mailboxes ADD COLUMN user_id TEXT NOT NULL DEFAULT 'owner';

ALTER TABLE threads ADD COLUMN user_id TEXT NOT NULL DEFAULT 'owner';
DROP INDEX threads_folder_last;
DROP INDEX threads_sender;
DROP INDEX threads_sent;
CREATE INDEX threads_folder_last ON threads (user_id, folder, last_at DESC);
CREATE INDEX threads_sender ON threads (user_id, sender_email, folder);
CREATE INDEX threads_sent ON threads (user_id, last_sent_at DESC) WHERE last_sent_at IS NOT NULL;

-- One stored copy per incoming Message-ID per user: mail to two people here is stored for each.
ALTER TABLE messages ADD COLUMN user_id TEXT NOT NULL DEFAULT 'owner';
DROP INDEX messages_message_id;
DROP INDEX messages_incoming_message_id;
CREATE INDEX messages_message_id ON messages (user_id, message_id);
CREATE UNIQUE INDEX messages_incoming_message_id ON messages (user_id, message_id) WHERE outgoing = 0 AND message_id IS NOT NULL;

-- Screening decisions and settings are per user; SQLite can't change a primary key in place.
CREATE TABLE senders_new (
  user_id TEXT NOT NULL,
  email TEXT NOT NULL,
  name TEXT,
  status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'blocked')),
  decided_at INTEGER,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (user_id, email)
);
INSERT INTO senders_new (user_id, email, name, status, decided_at, created_at)
  SELECT 'owner', email, name, status, decided_at, created_at FROM senders;
DROP TABLE senders;
ALTER TABLE senders_new RENAME TO senders;

CREATE TABLE settings_new (
  user_id TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY (user_id, key)
);
INSERT INTO settings_new (user_id, key, value) SELECT 'owner', key, value FROM settings;
DROP TABLE settings;
ALTER TABLE settings_new RENAME TO settings;
