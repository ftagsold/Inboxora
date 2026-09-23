-- Unified CalDAV/CardDAV servers: one calendar_import_sources row now optionally
-- carries an address-book side alongside its existing calendar side, so one
-- remote server (one set of credentials) can be managed as a single entity
-- instead of two separately-configured accounts.
--
-- address_book_collections mirrors `collections` (same whole-value encryption,
-- same [{href,hrefHash,displayName,selected}] shape, minus `color` — address
-- books don't have one). NULL means "this server has no address books" —
-- there is no legacy single-book fallback the way `collections`/`url` has for
-- calendars, since every unified server row is created discovery-first.
--
-- The address-book sync status is tracked independently of the calendar side's
-- last_sync_at/last_error so one concern's failure never hides the other's.
ALTER TABLE calendar_import_sources ADD COLUMN IF NOT EXISTS address_book_collections TEXT;
ALTER TABLE calendar_import_sources ADD COLUMN IF NOT EXISTS address_book_last_sync_at TIMESTAMPTZ;
ALTER TABLE calendar_import_sources ADD COLUMN IF NOT EXISTS address_book_last_error TEXT;
