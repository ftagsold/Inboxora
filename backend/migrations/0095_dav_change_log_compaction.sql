-- DAV change logs keep one payload-free row per resource (its latest version); sync REPORTs read
-- the current calendar_events / contacts row. Tombstones are pruned behind sync_floor: a token
-- older than the floor is rejected with valid-sync-token and the client resyncs fully.
--
-- Before this, every UPDATE (including no-op refreshes of subscribed calendars and CardDAV
-- books) appended a full iCalendar/vCard copy that was never removed.

ALTER TABLE calendars ADD COLUMN IF NOT EXISTS sync_floor BIGINT NOT NULL DEFAULT 0;
ALTER TABLE address_books ADD COLUMN IF NOT EXISTS sync_floor BIGINT NOT NULL DEFAULT 0;

-- ── calendar_sync_changes: rebuild with the latest row per resource, without raw_ical ──
CREATE TABLE calendar_sync_changes_compact (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  calendar_id UUID NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
  uid TEXT NOT NULL,
  recurrence_id TEXT NOT NULL DEFAULT '',
  version BIGINT NOT NULL,
  etag TEXT,
  deleted BOOLEAN NOT NULL DEFAULT false,
  dav_filename TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (calendar_id, uid, recurrence_id)
);
INSERT INTO calendar_sync_changes_compact (calendar_id, uid, recurrence_id, version, etag, deleted, dav_filename, created_at)
SELECT DISTINCT ON (calendar_id, uid, recurrence_id) calendar_id, uid, recurrence_id, version, etag, deleted, dav_filename, created_at
  FROM calendar_sync_changes
 ORDER BY calendar_id, uid, recurrence_id, version DESC;
DROP TABLE calendar_sync_changes;
ALTER TABLE calendar_sync_changes_compact RENAME TO calendar_sync_changes;
CREATE INDEX calendar_sync_changes_incremental_idx ON calendar_sync_changes (calendar_id, version);

-- ── contact_sync_changes: same, keyed by file name, without vcard ──
CREATE TABLE contact_sync_changes_compact (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  address_book_id UUID NOT NULL REFERENCES address_books(id) ON DELETE CASCADE,
  filename TEXT NOT NULL,
  version BIGINT NOT NULL,
  etag TEXT,
  deleted BOOLEAN NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (address_book_id, filename)
);
INSERT INTO contact_sync_changes_compact (address_book_id, filename, version, etag, deleted)
SELECT DISTINCT ON (address_book_id, filename) address_book_id, filename, version, etag, deleted
  FROM contact_sync_changes
 ORDER BY address_book_id, filename, version DESC;
DROP TABLE contact_sync_changes;
ALTER TABLE contact_sync_changes_compact RENAME TO contact_sync_changes;
CREATE INDEX contact_sync_changes_incremental_idx ON contact_sync_changes (address_book_id, version);

-- ── calendar trigger: upsert one row per resource; tombstone the old calendar on a move ──
CREATE OR REPLACE FUNCTION record_calendar_sync_change() RETURNS TRIGGER AS $$
DECLARE
  next_version BIGINT;
BEGIN
  IF TG_OP = 'DELETE' OR (TG_OP = 'UPDATE' AND (OLD.calendar_id, OLD.uid, OLD.recurrence_id)
                                     IS DISTINCT FROM (NEW.calendar_id, NEW.uid, NEW.recurrence_id)) THEN
    UPDATE calendars SET sync_version = sync_version + 1, sync_token = 'sync-' || (sync_version + 1)::text, updated_at = NOW()
     WHERE id = OLD.calendar_id RETURNING sync_version INTO next_version;
    IF FOUND THEN
      INSERT INTO calendar_sync_changes (calendar_id, uid, recurrence_id, version, etag, deleted, dav_filename)
      VALUES (OLD.calendar_id, OLD.uid, OLD.recurrence_id, next_version, NULL, true, OLD.dav_filename)
      ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE
        SET version = EXCLUDED.version, etag = NULL, deleted = true, dav_filename = EXCLUDED.dav_filename, created_at = NOW();
    END IF;
  END IF;
  IF TG_OP = 'DELETE' THEN
    RETURN OLD;
  END IF;
  UPDATE calendars SET sync_version = sync_version + 1, sync_token = 'sync-' || (sync_version + 1)::text, updated_at = NOW()
   WHERE id = NEW.calendar_id RETURNING sync_version INTO next_version;
  IF FOUND THEN
    INSERT INTO calendar_sync_changes (calendar_id, uid, recurrence_id, version, etag, deleted, dav_filename)
    VALUES (NEW.calendar_id, NEW.uid, NEW.recurrence_id, next_version, NEW.etag, false, NEW.dav_filename)
    ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE
      SET version = EXCLUDED.version, etag = EXCLUDED.etag, deleted = false, dav_filename = EXCLUDED.dav_filename, created_at = NOW();
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS calendar_events_sync_change ON calendar_events;
CREATE TRIGGER calendar_events_sync_change_write
  AFTER INSERT OR DELETE ON calendar_events
  FOR EACH ROW EXECUTE FUNCTION record_calendar_sync_change();
CREATE TRIGGER calendar_events_sync_change_update
  AFTER UPDATE ON calendar_events
  FOR EACH ROW
  WHEN ((OLD.calendar_id, OLD.uid, OLD.recurrence_id, OLD.etag, OLD.dav_filename, OLD.raw_ical)
        IS DISTINCT FROM (NEW.calendar_id, NEW.uid, NEW.recurrence_id, NEW.etag, NEW.dav_filename, NEW.raw_ical))
  EXECUTE FUNCTION record_calendar_sync_change();

-- Occurrences only need rebuilding when something they are projected from changed.
DROP TRIGGER IF EXISTS calendar_events_occurrences_dirty ON calendar_events;
CREATE TRIGGER calendar_events_occurrences_dirty_insert
  AFTER INSERT ON calendar_events
  FOR EACH ROW EXECUTE FUNCTION mark_calendar_occurrences_dirty();
CREATE TRIGGER calendar_events_occurrences_dirty_update
  AFTER UPDATE ON calendar_events
  FOR EACH ROW
  WHEN ((OLD.raw_ical, OLD.etag, OLD.calendar_id, OLD.starts_at, OLD.ends_at, OLD.all_day, OLD.timezone, OLD.summary,
         OLD.description, OLD.location, OLD.url, OLD.organizer, OLD.attendees)
        IS DISTINCT FROM (NEW.raw_ical, NEW.etag, NEW.calendar_id, NEW.starts_at, NEW.ends_at, NEW.all_day, NEW.timezone,
         NEW.summary, NEW.description, NEW.location, NEW.url, NEW.organizer, NEW.attendees))
  EXECUTE FUNCTION mark_calendar_occurrences_dirty();

-- ── contact trigger: same shape, keyed by DAV file name ──
CREATE OR REPLACE FUNCTION record_contact_sync_change() RETURNS TRIGGER AS $$
DECLARE
  next_version BIGINT;
BEGIN
  IF TG_OP = 'DELETE' OR (TG_OP = 'UPDATE' AND (OLD.address_book_id IS DISTINCT FROM NEW.address_book_id
      OR COALESCE(OLD.dav_filename, OLD.uid || '.vcf') IS DISTINCT FROM COALESCE(NEW.dav_filename, NEW.uid || '.vcf'))) THEN
    UPDATE address_books SET sync_version = sync_version + 1, sync_token = gen_random_uuid()::text, updated_at = NOW()
     WHERE id = OLD.address_book_id RETURNING sync_version INTO next_version;
    IF FOUND THEN
      INSERT INTO contact_sync_changes (address_book_id, filename, version, etag, deleted)
      VALUES (OLD.address_book_id, COALESCE(OLD.dav_filename, OLD.uid || '.vcf'), next_version, NULL, true)
      ON CONFLICT (address_book_id, filename) DO UPDATE
        SET version = EXCLUDED.version, etag = NULL, deleted = true, created_at = NOW();
    END IF;
  END IF;
  IF TG_OP = 'DELETE' THEN
    RETURN OLD;
  END IF;
  UPDATE address_books SET sync_version = sync_version + 1, sync_token = gen_random_uuid()::text, updated_at = NOW()
   WHERE id = NEW.address_book_id RETURNING sync_version INTO next_version;
  IF FOUND THEN
    INSERT INTO contact_sync_changes (address_book_id, filename, version, etag, deleted)
    VALUES (NEW.address_book_id, COALESCE(NEW.dav_filename, NEW.uid || '.vcf'), next_version, NEW.etag, false)
    ON CONFLICT (address_book_id, filename) DO UPDATE
      SET version = EXCLUDED.version, etag = EXCLUDED.etag, deleted = false, created_at = NOW();
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS contacts_sync_change ON contacts;
CREATE TRIGGER contacts_sync_change_write
  AFTER INSERT OR DELETE ON contacts
  FOR EACH ROW EXECUTE FUNCTION record_contact_sync_change();
CREATE TRIGGER contacts_sync_change_update
  AFTER UPDATE ON contacts
  FOR EACH ROW
  WHEN ((OLD.address_book_id, OLD.uid, OLD.dav_filename, OLD.etag, OLD.vcard)
        IS DISTINCT FROM (NEW.address_book_id, NEW.uid, NEW.dav_filename, NEW.etag, NEW.vcard))
  EXECUTE FUNCTION record_contact_sync_change();
