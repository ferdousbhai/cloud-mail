CREATE TABLE senders (
  email TEXT PRIMARY KEY,
  name TEXT,
  status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'blocked')),
  decided_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE threads (
  id TEXT PRIMARY KEY,
  subject TEXT NOT NULL,
  folder TEXT NOT NULL CHECK (folder IN ('screener', 'inbox', 'archive', 'blocked')),
  sender_email TEXT,
  from_name TEXT,
  from_email TEXT,
  to_address TEXT,
  snippet TEXT,
  message_count INTEGER NOT NULL DEFAULT 0,
  unread INTEGER NOT NULL DEFAULT 0,
  has_attachments INTEGER NOT NULL DEFAULT 0,
  last_sent_at INTEGER,
  last_at INTEGER NOT NULL
);
CREATE INDEX threads_folder_last ON threads (folder, last_at DESC);
CREATE INDEX threads_sender ON threads (sender_email, folder);
CREATE INDEX threads_sent ON threads (last_sent_at DESC) WHERE last_sent_at IS NOT NULL;

CREATE TABLE messages (
  id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
  outgoing INTEGER NOT NULL DEFAULT 0,
  message_id TEXT,
  in_reply_to TEXT,
  refs TEXT,
  from_name TEXT,
  from_email TEXT NOT NULL,
  to_json TEXT NOT NULL DEFAULT '[]',
  cc_json TEXT NOT NULL DEFAULT '[]',
  reply_to_json TEXT NOT NULL DEFAULT '[]',
  envelope_to TEXT,
  subject TEXT NOT NULL,
  text_body TEXT,
  html_body TEXT,
  html_key TEXT,
  date INTEGER NOT NULL,
  raw_key TEXT
);
CREATE INDEX messages_thread ON messages (thread_id, date);
CREATE INDEX messages_message_id ON messages (message_id);

CREATE TABLE attachments (
  id TEXT PRIMARY KEY,
  message_id TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
  filename TEXT NOT NULL,
  mime_type TEXT NOT NULL,
  size INTEGER NOT NULL,
  content_id TEXT,
  inline INTEGER NOT NULL DEFAULT 0,
  r2_key TEXT NOT NULL
);
CREATE INDEX attachments_message ON attachments (message_id);

CREATE VIRTUAL TABLE messages_fts USING fts5 (
  subject, body, from_text,
  tokenize = 'unicode61 remove_diacritics 2'
);
