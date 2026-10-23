-- IMAP connection limits, message change sequence and delete tombstones.
--
-- Connection limits: every account starts at its server's start value (admin-set here, else
-- IMAP_MAX_CONNECTIONS_PER_ACCOUNT, default 10) and learns a lower limit when the server
-- refuses connections. The learned value is kept on the account; an admin resets it.
ALTER TABLE email_accounts
  ADD COLUMN IF NOT EXISTS imap_conn_limit SMALLINT,
  ADD COLUMN IF NOT EXISTS imap_conn_limit_at TIMESTAMPTZ,
  ADD COLUMN IF NOT EXISTS imap_conn_limit_reason TEXT;

CREATE TABLE IF NOT EXISTS imap_server_limits (
  host TEXT NOT NULL,
  port INTEGER NOT NULL,
  start_limit SMALLINT CHECK (start_limit IS NULL OR start_limit BETWEEN 2 AND 50),
  auto_lower BOOLEAN NOT NULL DEFAULT true,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_by UUID REFERENCES users(id) ON DELETE SET NULL,
  PRIMARY KEY (host, port)
);

-- Change sequence: bumped whenever a row's list-visible state changes, so clients refresh a
-- view with the rows changed since their last sequence instead of reloading it.
CREATE SEQUENCE IF NOT EXISTS messages_change_seq;
ALTER TABLE messages ADD COLUMN IF NOT EXISTS change_seq BIGINT;
UPDATE messages SET change_seq = nextval('messages_change_seq') WHERE change_seq IS NULL;
ALTER TABLE messages ALTER COLUMN change_seq SET DEFAULT nextval('messages_change_seq');
ALTER TABLE messages ALTER COLUMN change_seq SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_messages_account_change_seq ON messages (account_id, change_seq);

CREATE OR REPLACE FUNCTION messages_bump_change_seq() RETURNS trigger AS $$
BEGIN
  IF NEW.is_read IS DISTINCT FROM OLD.is_read
     OR NEW.is_starred IS DISTINCT FROM OLD.is_starred
     OR NEW.folder IS DISTINCT FROM OLD.folder
     OR NEW.uid IS DISTINCT FROM OLD.uid
     OR NEW.is_deleted IS DISTINCT FROM OLD.is_deleted
     OR NEW.category IS DISTINCT FROM OLD.category
     -- thread_key is generated from thread_id (and not yet computed in a BEFORE trigger).
     OR NEW.thread_id IS DISTINCT FROM OLD.thread_id
     OR NEW.has_attachments IS DISTINCT FROM OLD.has_attachments
     OR NEW.date IS DISTINCT FROM OLD.date THEN
    NEW.change_seq := nextval('messages_change_seq');
  END IF;
  RETURN NEW;
END $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS messages_change_seq ON messages;
CREATE TRIGGER messages_change_seq BEFORE UPDATE ON messages
  FOR EACH ROW EXECUTE FUNCTION messages_bump_change_seq();

-- Tombstones: ids only, pruned after a day. The janitor drops each deleted row's cached
-- display fields and body from Redis; clients learn removed rows from the same sequence.
CREATE TABLE IF NOT EXISTS message_tombstones (
  seq BIGINT PRIMARY KEY DEFAULT nextval('messages_change_seq'),
  id UUID NOT NULL,
  account_id UUID NOT NULL,
  folder TEXT NOT NULL,
  deleted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  cache_cleared BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX IF NOT EXISTS idx_message_tombstones_account_seq ON message_tombstones (account_id, seq);
CREATE INDEX IF NOT EXISTS idx_message_tombstones_pending ON message_tombstones (seq) WHERE NOT cache_cleared;

CREATE OR REPLACE FUNCTION messages_tombstone() RETURNS trigger AS $$
BEGIN
  INSERT INTO message_tombstones (id, account_id, folder)
    SELECT o.id, o.account_id, o.folder FROM old_rows o;
  RETURN NULL;
END $$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS messages_tombstone ON messages;
CREATE TRIGGER messages_tombstone AFTER DELETE ON messages
  REFERENCING OLD TABLE AS old_rows
  FOR EACH STATEMENT EXECUTE FUNCTION messages_tombstone();
