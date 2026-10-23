-- Mail content leaves PostgreSQL. `messages` becomes a header index: where a message lives
-- (account, folder, UID), its date and flags, threading links and app-owned state. Subjects,
-- addresses, snippets, bodies and raw headers are fetched live from IMAP and cached in Redis
-- for a few minutes (engine/cache.rs). The threading engine keeps hashes instead of text.

-- messages → logical_messages/conversations are DEFERRABLE foreign keys; check them as each
-- statement runs so the ALTER TABLEs below don't meet pending trigger events.
SET CONSTRAINTS ALL IMMEDIATE;

-- ── messages ──
ALTER TABLE messages
  DROP COLUMN IF EXISTS search_vector,
  DROP COLUMN IF EXISTS normalized_subject,
  DROP COLUMN IF EXISTS subject,
  DROP COLUMN IF EXISTS from_name,
  DROP COLUMN IF EXISTS from_email,
  DROP COLUMN IF EXISTS to_addresses,
  DROP COLUMN IF EXISTS cc_addresses,
  DROP COLUMN IF EXISTS reply_to,
  DROP COLUMN IF EXISTS snippet,
  DROP COLUMN IF EXISTS snippet_attempted_at,
  DROP COLUMN IF EXISTS body_text,
  DROP COLUMN IF EXISTS body_html,
  DROP COLUMN IF EXISTS attachments,
  DROP COLUMN IF EXISTS in_reply_to,
  DROP COLUMN IF EXISTS thread_references,
  DROP COLUMN IF EXISTS list_unsubscribe,
  DROP COLUMN IF EXISTS list_unsubscribe_post,
  DROP COLUMN IF EXISTS delivery_addresses,
  DROP COLUMN IF EXISTS sender_email,
  DROP COLUMN IF EXISTS sender_name,
  DROP COLUMN IF EXISTS spam_details,
  DROP COLUMN IF EXISTS conversation_raw_headers,
  DROP COLUMN IF EXISTS conversation_thread_index,
  DROP COLUMN IF EXISTS conversation_thread_topic;

-- Indexes that only served removed columns or duplicate a narrower index.
DROP INDEX IF EXISTS idx_messages_body;
DROP INDEX IF EXISTS idx_messages_search;
DROP INDEX IF EXISTS idx_messages_read;
DROP INDEX IF EXISTS idx_messages_date;
DROP INDEX IF EXISTS idx_messages_msg_id;
DROP INDEX IF EXISTS idx_messages_account_folder;

-- ── logical_messages: hashes for the comparisons threading makes, no text ──
ALTER TABLE logical_messages
  ADD COLUMN IF NOT EXISTS subject_hash TEXT,
  ADD COLUMN IF NOT EXISTS references_anchor TEXT,
  ADD COLUMN IF NOT EXISTS series_sender_hash TEXT,
  ADD COLUMN IF NOT EXISTS series_recipients_hash TEXT,
  ADD COLUMN IF NOT EXISTS series_automated BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS body_template_fingerprint TEXT;

UPDATE logical_messages SET subject_hash = left(encode(sha256(convert_to(lower(canonical_subject), 'UTF8')), 'hex'), 16)
 WHERE canonical_subject IS NOT NULL AND canonical_subject <> '';
UPDATE logical_messages
   SET references_anchor = COALESCE(parsed_references->>0, parsed_in_reply_to->>(jsonb_array_length(parsed_in_reply_to) - 1))
 WHERE jsonb_typeof(parsed_references) = 'array' AND jsonb_typeof(parsed_in_reply_to) = 'array';

DROP INDEX IF EXISTS idx_logical_messages_canonical_subject;
ALTER TABLE logical_messages
  DROP COLUMN IF EXISTS raw_headers,
  DROP COLUMN IF EXISTS raw_message_id,
  DROP COLUMN IF EXISTS raw_in_reply_to,
  DROP COLUMN IF EXISTS raw_references,
  DROP COLUMN IF EXISTS parsed_in_reply_to,
  DROP COLUMN IF EXISTS parsed_references,
  DROP COLUMN IF EXISTS subject,
  DROP COLUMN IF EXISTS canonical_subject,
  DROP COLUMN IF EXISTS from_address,
  DROP COLUMN IF EXISTS sender_address,
  DROP COLUMN IF EXISTS recipient_signature,
  DROP COLUMN IF EXISTS sender_signature,
  DROP COLUMN IF EXISTS diagnostics;

-- ── conversations: the subject is read from the first message's cached headers ──
ALTER TABLE conversations
  DROP COLUMN IF EXISTS subject_snapshot,
  DROP COLUMN IF EXISTS canonical_subject;

-- Routine evidence duplicated messages.threading_reason for every message; only the rare
-- ambiguous-parent and message-id-collision records carry information worth keeping.
DELETE FROM conversation_evidence WHERE evidence_type NOT IN ('ambiguous-parent', 'message-id-collision');

-- Invitations received by mail are re-read from the message's .ics part when shown.
ALTER TABLE inbound_calendar_invitations
  DROP COLUMN IF EXISTS raw_ical,
  DROP COLUMN IF EXISTS summary,
  DROP COLUMN IF EXISTS organizer;
