-- The reply (accepted / tentative / declined) a user sent to a received calendar invitation.
-- Keyed by the invitation identity (UID, recurrence, organizer) rather than the mail message,
-- so the answer stays known after the message is archived or re-keyed by a folder move.
-- `sequence` is the invitation revision the reply answered: a later update from the organizer
-- (higher SEQUENCE) asks again, so an older reply is not shown as current for it.
CREATE TABLE IF NOT EXISTS calendar_invitation_responses (
  user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  uid TEXT NOT NULL,
  recurrence_id TEXT NOT NULL DEFAULT '',
  organizer TEXT NOT NULL DEFAULT '',
  sequence INTEGER NOT NULL DEFAULT 0,
  response TEXT NOT NULL CHECK (response IN ('ACCEPTED', 'TENTATIVE', 'DECLINED')),
  responded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (user_id, uid, recurrence_id, organizer)
)
