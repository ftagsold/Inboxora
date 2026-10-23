-- Bumped on every edit of a scheduled mail: the send's idempotency key includes it, so an
-- edited mail is a new message rather than a replay of the old one.
ALTER TABLE scheduled_sends ADD COLUMN IF NOT EXISTS revision INTEGER NOT NULL DEFAULT 0;
