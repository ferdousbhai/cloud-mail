-- One token per machine: a user's second computer gets its own (`cloudmail machine add`, or the
-- admin's `cloudmail setup` there) instead of a copy of, or a replacement for, the first one's.
-- users.token_hash is no longer read; it stays so a worker one version back keeps working until
-- the new one is deployed.
CREATE TABLE tokens (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  -- SHA-256 (hex) of the API token.
  token_hash TEXT NOT NULL UNIQUE,
  -- What the machine called itself when it connected (its hostname); may be empty.
  name TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL
);
CREATE INDEX tokens_user ON tokens (user_id, created_at);
INSERT INTO tokens (id, user_id, token_hash, created_at)
  SELECT 'k_' || id, id, token_hash, created_at FROM users WHERE token_hash IS NOT NULL AND revoked_at IS NULL;
