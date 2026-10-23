# Release notes 5.0.0

**Status:** in development (not released) · **Previous version:** 4.0.2

## Why 5.0.0

5.0.0 replaces the Node.js backend with a rewrite in Rust. The interface, the Android app, DAV
clients and the database do not change, and the HTTP API stays wire-compatible. Even so, the
server that holds your mail connections, sessions and credentials is new code, and one feature
area — plugins — is gone. That is a major version by any reading of semantic versioning.

What the rewrite is for:

- **Lower and steadier resource use.** One async process holds every IMAP IDLE connection, sync
  timer and WebSocket without a garbage collector. Measured figures against 4.x will be added
  here before the release.
- **A mail engine that is ours.** A purpose-built IMAP client (TLS, OAuth2, COMPRESS, CONDSTORE,
  UIDPLUS, MOVE, IDLE) replaces the imapflow dependency. It is tested against a real Dovecot
  server.
- **A smaller attack surface.** The image is distroless: no shell, no package manager,
  non-root.
- **One container.** The Rust server also serves the web app, terminates HTTPS and proxies the
  bundled ntfy. The separate nginx frontend container and image are gone.

## Upgrading

1. Back up the database as usual (see [Upgrading](Upgrading.md)).
2. Pull the 5.0.0 images, or rebuild with `docker compose build`, and start the stack. The new
   backend applies any pending migrations itself at startup, exactly as 4.x did.
3. Existing sessions stay valid: the session cookie, Redis session format and encryption keys
   are unchanged, so nobody is signed out.

Environment variables are the same as in 4.x. If you run your own healthcheck against the
container, note that the image has no `wget` or shell: use
`/app/inboxora-server healthcheck`, or probe `GET /api/health` from outside the container.

The `frontend` service is gone: take the new `docker-compose.yml` / `docker-compose.ghcr.yml`
(or, with your own compose file, delete the `frontend` service and move its `ports`, the
`./certs` volume and `VITE_APP_NAME` to `backend`, mapping `APP_PORT` to container port 3443 and
`APP_HTTP_PORT` to 3000, and add `NTFY_URL: http://ntfy:80`). The existing `./certs` keeps
working. A reverse proxy that pointed at the frontend container's port 80 now points at the
Inboxora container's port 3000; a Caddyfile that proxied `frontend:443` proxies `backend:3000`.
`ghcr.io/dragonk/inboxora-frontend` is no longer published.

### Storage: smaller database, faster backups

5.0.0 stops copying mail into PostgreSQL. The database keeps a header index (where each message
lives, its date, flags and threading links); subjects, addresses, snippets and bodies are read
from your IMAP server and cached in Redis: every folder's subjects, senders and snippets are
preloaded after each start (the web app shows a short "your account is ready shortly" screen
meanwhile) and bodies are kept for seven days (`MAIL_BODY_TTL_SECONDS`). Search runs
on the mail server. Calendar subscriptions, read-only CalDAV calendars and CardDAV address books
are likewise read live; two-way CalDAV calendars and everything created in Inboxora stay stored.

The first start after the upgrade drops the old copies and then rewrites the affected tables
once (`VACUUM FULL`) so the space actually returns to the disk. Expect a longer first start on a
large installation, and have free disk space about the size of the `messages` table; the log
reports each table and the database size before and after. If the rewrite fails (for example
for lack of space) it is retried on the next start.

Because mail is read live, a cold mailbox page needs one IMAP round trip, and search speed
depends on your provider's server-side search.

## What is gone

- **Plugins.** The GTD plugin (labels, sections, the pet) and the plugin hooks are not part of
  5.0.0. `/api/plugins` reports no plugins. Plugin data already in the database is not deleted.

## Behaviour differences worth knowing

The rewrite reproduces 4.x behaviour. A differential test suite runs both backends against
identical databases and compares their responses, side effects (mail sent, rows written) and
headers across scenarios for every route group. The deliberate differences are listed in
`backend/README.md`. The ones a user can notice are:

- A few 4.x bugs are fixed: search `after:`/`before:`, contacts with a title, the conversation
  rebuild rate limit (now `429`), and raw headers in threading diagnostics.
- If your SMTP server rejects one recipient of a message, the send fails as a whole and can be
  retried. 4.x could report a partial delivery instead.

## Known limitations

- The Node backend has been removed from the repository; `backend/` is the Rust workspace. Its
  recorded outputs remain in `backend/tests/differential` as regression fixtures.
