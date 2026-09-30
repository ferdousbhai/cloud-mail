-- Plain-text bodies too large for a D1 row live in R2, like large HTML bodies.
ALTER TABLE messages ADD COLUMN text_key TEXT;
