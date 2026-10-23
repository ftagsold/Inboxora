-- no-transaction
-- Index audit for the header-index schema: every foreign key gets an index its referential
-- checks can use (a partial index on `is_deleted = false` cannot serve them, so deleting an
-- orphaned conversation or logical message scanned all of `messages`), and indexes that
-- duplicate a wider one or serve no query are dropped. Each index costs disk and write time.

-- messages: FK-usable replacements for the partial conversation / logical-message indexes.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_messages_conversation_fk
  ON messages (conversation_id, account_id, date DESC) WHERE conversation_id IS NOT NULL;
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_conversation;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_messages_logical_fk
  ON messages (logical_message_id, date DESC NULLS LAST, id DESC) WHERE logical_message_id IS NOT NULL;
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_logical_message_date;

-- messages: (account_id, folder, thread_key, …) is covered by idx_messages_threaded_dedup and
-- (account_id, thread_key) by idx_messages_thread_count; nothing filters on spam_verdict or on
-- the provider thread id of a copy (provider_thread_mappings serves that lookup).
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_thread_key;
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_thread_key_lookup;
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_spam_verdict;
DROP INDEX CONCURRENTLY IF EXISTS idx_messages_provider_thread;

-- logical_messages: every lookup is per account; the unique (user, account, canonical id,
-- collision key) index serves them all. (id, user_id) duplicates the primary key and no
-- foreign key references it.
DROP INDEX CONCURRENTLY IF EXISTS idx_logical_messages_collision;
DROP INDEX CONCURRENTLY IF EXISTS idx_logical_messages_user_account_message;
DROP INDEX CONCURRENTLY IF EXISTS idx_logical_messages_user_message;
ALTER TABLE logical_messages DROP CONSTRAINT IF EXISTS uq_logical_messages_id_user;
ALTER TABLE conversations DROP CONSTRAINT IF EXISTS uq_conversations_id_user;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_logical_messages_parent
  ON logical_messages (parent_logical_message_id) WHERE parent_logical_message_id IS NOT NULL;

-- Series detection compares 64-bit digests; the full SHA-256 only made the index wider.
DROP INDEX CONCURRENTLY IF EXISTS idx_logical_messages_subject_hash;
UPDATE logical_messages SET subject_hash = left(subject_hash, 16) WHERE length(subject_hash) > 16;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_logical_messages_series
  ON logical_messages (account_id, subject_hash) WHERE subject_hash IS NOT NULL;

-- Foreign keys whose referencing side had no usable index.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_conversation_ingest_failures_row
  ON conversation_ingest_failures (message_row_id) WHERE message_row_id IS NOT NULL;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_unresolved_resolved
  ON unresolved_message_references (resolved_logical_message_id) WHERE resolved_logical_message_id IS NOT NULL;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_conversation_aliases_canonical
  ON conversation_aliases (canonical_conversation_id);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_conversation_evidence_logical
  ON conversation_evidence (logical_message_id);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_provider_thread_mappings_conversation
  ON provider_thread_mappings (conversation_id);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_conversations_continued_from
  ON conversations (continued_from_conversation_id) WHERE continued_from_conversation_id IS NOT NULL;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_conversations_continued_to
  ON conversations (continued_to_conversation_id) WHERE continued_to_conversation_id IS NOT NULL;

-- Fingerprints only identify copies without a Message-ID, and a 64-bit collision key is
-- plenty to tell apart messages sharing one; both were 64-character hex on every row.
UPDATE logical_messages SET body_fingerprint = NULL, header_fingerprint = NULL
 WHERE canonical_message_id IS NOT NULL AND (body_fingerprint IS NOT NULL OR header_fingerprint IS NOT NULL);
UPDATE logical_messages SET message_id_collision_key = left(message_id_collision_key, 16)
 WHERE length(message_id_collision_key) > 16;
