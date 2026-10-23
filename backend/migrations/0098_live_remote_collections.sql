-- Read-only remote collections — iCalendar feeds, read-only CalDAV calendars and CardDAV address
-- books — are served live from the remote server (services/remote_dav.rs) and cached in Redis.
-- Drop the copies earlier versions synced into PostgreSQL. Two-way CalDAV calendars stay stored.

DELETE FROM calendar_events e USING calendars c
 WHERE c.id = e.calendar_id AND c.source IN ('ical_url', 'caldav') AND c.read_only;
DELETE FROM calendar_sync_changes ch USING calendars c
 WHERE c.id = ch.calendar_id AND c.source IN ('ical_url', 'caldav') AND c.read_only;

DELETE FROM contacts ct USING address_books b
 WHERE b.id = ct.address_book_id AND b.source = 'carddav';
DELETE FROM contact_sync_changes ch USING address_books b
 WHERE b.id = ch.address_book_id AND b.source = 'carddav';

-- The full feed document was a third copy of every subscribed event.
DROP TABLE IF EXISTS calendar_import_documents;
