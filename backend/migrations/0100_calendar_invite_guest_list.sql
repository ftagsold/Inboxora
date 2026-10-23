-- Hidden guest lists: each attendee's invitation names only them.
ALTER TABLE calendar_events
  ADD COLUMN IF NOT EXISTS invite_hide_attendees BOOLEAN NOT NULL DEFAULT false;
