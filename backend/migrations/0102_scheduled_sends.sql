-- Mail scheduled for later delivery. `payload` is the POST /mail/send body as submitted
-- (base64 attachments included); the row is deleted once the message is sent and kept as
-- `failed` (with `last_error`) when sending did not succeed, until the user discards it.
CREATE TABLE IF NOT EXISTS scheduled_sends (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  account_id UUID NOT NULL REFERENCES email_accounts(id) ON DELETE CASCADE,
  send_at TIMESTAMPTZ NOT NULL,
  payload JSONB NOT NULL,
  status TEXT NOT NULL DEFAULT 'scheduled' CHECK (status IN ('scheduled', 'sending', 'failed')),
  last_error TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS scheduled_sends_due_idx ON scheduled_sends (send_at) WHERE status = 'scheduled';
CREATE INDEX IF NOT EXISTS scheduled_sends_user_idx ON scheduled_sends (user_id, account_id);
