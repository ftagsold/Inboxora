-- CalDAV sources become editable, discoverable, faster-polling, optionally
-- writable, and able to expose more than one remote calendar.

-- interval_min (whole minutes, 15-1440) -> interval_seconds (whole seconds, 15-86400).
ALTER TABLE calendar_import_sources RENAME COLUMN interval_min TO interval_seconds;
ALTER TABLE calendar_import_sources ALTER COLUMN interval_seconds DROP DEFAULT;
UPDATE calendar_import_sources SET interval_seconds = interval_seconds * 60;
ALTER TABLE calendar_import_sources ALTER COLUMN interval_seconds SET DEFAULT 3600;
ALTER TABLE calendar_import_sources DROP CONSTRAINT IF EXISTS calendar_import_sources_interval_min_check;
ALTER TABLE calendar_import_sources ADD CONSTRAINT calendar_import_sources_interval_seconds_check
  CHECK (interval_seconds BETWEEN 15 AND 86400);

-- Whether local edits/deletes on this source's calendar(s) are pushed back to the
-- remote server. ical_url sources are static feeds and stay permanently read-only
-- (enforced in application code, not just this default).
ALTER TABLE calendar_import_sources ADD COLUMN IF NOT EXISTS writable BOOLEAN NOT NULL DEFAULT false;

-- Discovered calendars for a CalDAV account that exposes more than one, as an
-- encrypted JSON blob (matching the whole-value encryption already used for
-- `url`/`password`): [{ href, hrefHash, displayName, color, selected }]. The
-- derived local `calendars` row is found by hrefHash, not stored here — see
-- externalCalendarSync.ts's external_url scheme.
-- NULL means "not discovered" — the source keeps syncing its single stored `url`
-- exactly as before, so pre-existing sources are unaffected by this migration.
ALTER TABLE calendar_import_sources ADD COLUMN IF NOT EXISTS collections TEXT;

-- The remote resource identity for an imported CalDAV event, captured from the
-- REPORT response (href + getetag). Only ever set for kind='caldav' sources;
-- required to PUT/DELETE back to the correct resource with the right If-Match.
ALTER TABLE calendar_events ADD COLUMN IF NOT EXISTS remote_href TEXT;
ALTER TABLE calendar_events ADD COLUMN IF NOT EXISTS remote_etag TEXT;
