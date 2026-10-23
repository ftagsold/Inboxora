# Inboxora backend (Rust)

The Inboxora backend from 5.0.0 on: a Rust rewrite of the 4.x Node backend, wire-compatible with
the frontend, the Android app, DAV clients and existing databases, which it upgrades in place. It
also serves the built web app and proxies the bundled ntfy, so a deployment is one container.
The migration plan and parity gates are in
[`docs/rust-backend-migration-plan.md`](../docs/rust-backend-migration-plan.md).

## Layout

| Crate | Contents |
|---|---|
| `crates/imap` | IMAP client on imap-proto: STARTTLS/implicit TLS, PLAIN/LOGIN/XOAUTH2/OAUTHBEARER, NAMESPACE, COMPRESS=DEFLATE, CONDSTORE, UIDPLUS, MOVE, IDLE, modified UTF-7, imapflow-compatible special-use and body-structure numbering |
| `crates/net` | SSRF-guarded HTTP client (validated DNS, redirect re-checks) and host validation |
| `crates/core` | config (Node env names/defaults), PostgreSQL pool + node-pg compatible params/rows, migration runner, express-session compatible sessions, `enc:v1` crypto, Redis, event fan-out, metrics |
| `crates/mail` | HTML sanitizer (sanitize-html 2.18 semantics), snippets, headers/RFC 2047, MIME parts, conversation helpers, small mail utilities |
| `crates/pim` | vCard 3.0, iCalendar (literal port of the ical.js 2.2 subset: parser, serialiser, VTIMEZONE offsets, RRULE iterator/expansion, events) and the calendar projection functions |
| `crates/server` | axum binary: Express-equivalent middleware stack, extractors, WebSocket, routes, the mail engine (`engine/`: persistent IDLE connections, pooled leases, sync, backfill, flag push), services (send, conversations, invitations, AI, push), the web app / ntfy front layer (`frontend.rs`) and the HTTPS listener (`tls.rs`) |

## Running

```bash
cargo build --release -p inboxora-server
# Same environment as the 4.x backend (DB_*, REDIS_URL, SESSION_SECRET, ENCRYPTION_KEY, APP_URL, …).
# It applies pending migrations at startup; MIGRATIONS=verify only checks the schema.
# MIGRATIONS_DIR defaults to ./migrations.
./target/release/inboxora-server
./target/release/inboxora-server healthcheck   # exit 0 when /api/health answers 200
```

Serving the web app, ntfy and HTTPS (all optional; the container image sets the first three):

| Variable | Effect |
|---|---|
| `STATIC_DIR` | Serve the built frontend (`frontend/dist`) for every GET/HEAD outside the backend's own paths (`/api`, `/ws`, `/oauth`, `/auth/oidc`, `/carddav`, `/caldav`, `/.well-known/{card,cal}dav`, `/calendar/feeds`), with `index.html` for client-side routes. `index.html` is never cached, hashed assets for a year. |
| `VITE_APP_NAME` | App name filled into `index.html` and `manifest.json` at startup (default `Inboxora`). |
| `TLS_PORT` | Also listen for HTTPS (HTTP/2 and HTTP/1.1 via ALPN; WebSockets use HTTP/1.1) on this port. The listener is the edge: it drops client X-Forwarded-For, sets X-Forwarded-Proto `https` and sends HSTS. |
| `TLS_CERT_FILE`, `TLS_KEY_FILE` | PEM certificate chain and key (default `certs/cert.pem`, `certs/key.pem`). When neither exists a self-signed pair is generated and written there. |
| `NTFY_URL` | Internal ntfy origin (e.g. `http://ntfy:80`). `/up` + 12 base62 characters and `/v1/*` are proxied unchanged, `/push/*` with the prefix stripped; WebSocket upgrades included. |

The plain HTTP listener (`PORT`) trusts one proxy hop for X-Forwarded-For / X-Forwarded-Proto,
like the 4.x backend; expose it only behind a TLS-terminating reverse proxy.

The container image (`backend/Dockerfile`, built from the repository root) builds the frontend
and the server and ships both on a distroless, non-root runtime: migrations at `/app/migrations`,
the web app at `/app/public`, HTTP on 3000 and HTTPS on 3443 with the certificate in
`/app/certs`:

```bash
docker build -f backend/Dockerfile -t inboxora .
```

## Tests

```bash
# Unit tests; integration tests run when TEST_DB_HOST/TEST_REDIS_URL are set (see .env.test).
set -a; . ./.env.test; set +a; cargo test
```

`.env.test` (not committed) holds `TEST_DB_HOST`, `TEST_DB_PORT`, `TEST_DB_PASSWORD`,
`TEST_REDIS_URL`. `tests/differential/*.json` are frozen recordings of the 4.x Node backend's
outputs (sanitizer, snippets, vCard, iCalendar, DAV XML) that the Rust crates must reproduce.
`tests/e2e/playwright-seed.sql` seeds the real-app Playwright fixture (see the
`conversation-v2-real-app-playwright` workflow).

## Porting status

| Phase | Area | Status |
|---|---|---|
| 0 | core, middleware stack, sessions, migrations, WebSocket, health/version | done |
| 1 | pure libraries (MIME, sanitizer, vCard, iCal/recurrence, threading) | done — differential corpora in `tests/differential` |
| 2 | database-only route groups (block list, contacts, search, rules, categories, push devices, calendar reads) | done |
| 3 | DAV servers and DAV sync | done — `/carddav`, `/caldav`, well-known redirects, app passwords, secret feeds, DAV clients, `/api/dav/servers`, `/api/calendar/sources`, remote sync with write-back, contact backfills. Calendar event mutations moved to phase 5 (they deliver invitations over SMTP) |
| 4 | authentication | done — `/api/auth` (password, TOTP, email OTP, lock, preferences, reset), `/api/totp`, OIDC sign-in, Microsoft OAuth, `/api/admin`, `/api/integrations`, SMTP mailer |
| 5 | mail engine, mail routes, send, WebSocket events, calendar event mutations and invitations | done — IMAP engine (live-tested against Dovecot), `/api/mail` (messages, folders, bulk actions, snooze, spam, unsubscribe, conversations with overrides and rebuilds, send and drafts), `/api/accounts`, calendar mutations with the invitation outbox, received invitations, Web Push/FCM/UnifiedPush dispatch |
| 6 | AI, integrations, cutover | done — AI providers (OpenAI-compatible API key and ChatGPT device OAuth, streaming chat), Todoist, sender favicons, `/api/diagnostics`, Docker image and compose/CI cutover |

Plugins (GTD) are out of scope for the rewrite.

Test hook: `MS_AUTHORITY_URL` overrides the Microsoft identity platform base URL (Node has it
fixed); production deployments leave it unset.

Background work: the server starts the mail engine (startup connections, sync timers, snippet
indexer, staleness and flag-push reconcilers, snooze watcher), the DAV/iCalendar schedulers, the
invitation outbox worker and the conversation ingest retry. `BACKGROUND_SYNC=off` keeps the mail
engine timers and DAV schedulers off; `NODE_ENV=test` or `E2E_DISABLE_IMAP_CONNECT=true` skips
the startup IMAP connections.

## Intentional deviations from the Node backend

Differential tests (`tests/differential/*.expected.json`, recorded from the Node code) must
match exactly except for these:

- A malformed JSON body returns `400 {"error":"Invalid JSON body"}` (Node returned a generic 500).
- `204` responses carry no `Content-Length` (forbidden by RFC 9110; hyper strips it).
- RFC 2047 encoded words are decoded with their declared charset (Node always decoded UTF-8,
  turning `=?iso-8859-1?Q?caf=E9?=` into `caf\ufffd`); adjacent words in one charset are decoded
  together so a character split across words survives.
- Quoted-printable bodies keep raw 8-bit bytes (Node's `ascii` decoding cleared the high bit).
- Out-of-range numeric entities in text snippets become U+FFFD instead of throwing.
- A CSS comment inside a `style` attribute is dropped (postcss turned it into
  `undefined:undefined`).
- Snippets and `htmlToPlainText` follow the HTML spec for raw-text/RCDATA/foreign elements
  (`iframe`, `textarea`, `svg`, …); Node used htmlparser2 10 there, while its sanitizer used 12.
- Recurrence rules whose expansion never terminates in ical.js (e.g. `FREQ=WEEKLY;BYWEEKNO=-1`,
  or contracting parts that can never match) stop after 1,000,000 inner steps with a rule error
  instead of spinning the process.
- Search `after:`/`before:` operators work (Node generated `m.date >= 3` without the `$`
  placeholder, so any search using them failed with 500).
- Creating a contact with a title works (Node cast the `title` parameter to `jsonb` instead of
  `contact_dates`, so `POST /api/contacts` with a title failed with 500).
- DAV client XML is parsed by a port of fast-xml-parser 5 (`services/dav_xml.rs`) so discovery
  and imports match Node exactly (see `tests/differential/dav.expected.json`), with one exception: DOCTYPE-declared entities are only expanded for simple
  internal declarations.
- CardDAV/CalDAV request bodies are capped at 16 MiB (Node read them unbounded) and answer 413
  beyond that.
- Plugins are not ported: `/api/plugins` lists none, account rows carry no `gtd_enabled` /
  `gtd_folders`, and GTD-only behaviour (label sibling rows, `onSentMessage` hooks, plugin
  summaries) does not exist.
- `conversation_raw_headers` stores the message's header text (Node stored a dump of its
  header-entry objects).
- The conversation rebuild rate limit answers `429` (Node's global error handler turned the
  limiter's error into a `500`).
- An SMTP server rejecting one recipient fails the whole send (the SMTP client does not continue
  with the accepted recipients), so `partialDelivery` is never reported.
- Drafts are appended with CRLF line endings like Sent copies (Node used bare LF for drafts).
- An unauthenticated request to an unknown `/api/ai/*` path answers `401` (Node `404`).
