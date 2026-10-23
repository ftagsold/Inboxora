# Rust backend migration plan

> **Historical.** The migration is complete: the Node backend has been removed and `backend-rs/`
> was renamed to `backend/`. Paths below are as they were during the port.

Status: proposal · Base: `main` @ `cb10bf12` · Backend version at time of writing: 4.0.2

## 1. Goal and non-goals

**Goal.** Replace the Node/TypeScript backend (`backend/`) with a Rust backend that is wire-compatible
with the existing frontend, Android app, DAV clients and deployed databases, and that is measurably
faster and leaner where the current backend spends CPU and memory.

**Non-goals.**

- No frontend changes beyond what a contract fix strictly requires.
- No schema redesign. The Rust backend uses the existing PostgreSQL schema and keeps adding
  migrations additively (AGENTS.md).
- No big-bang cutover. Node stays the production backend for every surface until the Rust
  replacement for that surface has passed the parity gates in §7.

## 2. What we are porting

| Area | Size / notes |
|---|---|
| Production TS | ~41.6k lines; tests ~28.1k lines |
| HTTP surface | ~250 handlers across 31 route files; largest: `mail.ts` (32), `auth.ts` (28), `conversations.ts` (21), `calendar.ts` (20), `admin.ts` (18) |
| IMAP engine | `services/imapManager.ts`, 6.1k lines: pooled + persistent IDLE connections, CONDSTORE/modseq deltas, per-host semaphores and cooldowns, flag-push reconciliation, snippet indexer, snooze watcher, move/copy reconciliation |
| Conversation engine v2 | engine, persistence, overrides, rebuild jobs, ingest retry; tenant-safety and race tests |
| Calendar | iCal parsing/recurrence (`ical.js`), projection worker pool (`worker_threads`), CalDAV client + server, invitation outbox, secret feeds |
| Contacts | vCard, CardDAV client + server, Google import |
| Auth | express-session in Redis, password + bcrypt, TOTP/MFA + device trust, OIDC RP (+ RP-initiated logout, mail passthrough), OAuth integrations, screen lock, DAV app passwords, device tokens |
| Outbound | SMTP (nodemailer) with send idempotency fingerprints, rule forwarder, web push / FCM / UnifiedPush |
| Other | HTML sanitizer (`sanitize-html`), SSRF-guarded fetch, AI providers (incl. OpenAI Codex OAuth), Todoist, plugins (GTD), diagnostics/perf metrics |
| Schema | 100 SQL migrations, checksummed runner with an advisory lock |

### Constraints from how the code works today

1. **The IMAP engine is in-process state, and most mail routes call it directly**
   (`app.set('imapManager')`). A mail route can't move to Rust without the engine, unless the
   route makes an RPC call back to Node. This drives the order of work (§6).
2. **Realtime broadcast is in-process.** `ImapManager.broadcast` writes to the `ws` server in the
   same process, and there's no Redis pub/sub today. Once two processes produce events, they need a
   shared event bus.
3. **Sessions are express-session/connect-redis.** Keys `sess:<id>` hold JSON. The cookie is
   `connect.sid=s:<id>.<base64 HMAC-SHA256(SESSION_SECRET, id), '=' padding stripped>` (URL-encoded). The
   `SessionData` fields are in `src/types/express-session.d.ts`. Rust must read and write this format
   exactly while both backends run.
4. **Stored-data formats that must stay byte-compatible:** `enc:v1:<iv>:<tag>:<ct>` AES-256-GCM
   credentials (`encryption.ts`), bcrypt `$2a/$2b` hashes, DAV ETags (md5/sha256 of the payload),
   send fingerprints (sha256 of `JSON.stringify` of specific shapes: key order matters),
   calendar UIDs derived from sha256 of JSON arrays, `schema_migrations.sha256`.
5. **The plugin boundary is TypeScript-specific** (an ESLint import rule plus a facade). Rust needs
   its own way to enforce it (§4.4).
6. **nginx already routes by path prefix** (`frontend/nginx.conf`: `/api/`, `/api/ai/`,
   `/carddav/`, `/caldav/`, `/ws`, `/oauth/`, `/auth/oidc/`). Running both backends side by side,
   each owning some paths, needs no new infrastructure.

## 3. Compatibility contract (must not change)

- Every HTTP path, method, status code, JSON shape and error message the frontend or Android app
  relies on, including the 401/403/413/423 semantics, the `X-Requested-With` CSRF gate, the lock
  allow-list, and security headers.
- Session semantics: `requireAuth` checks the user row on **every** request and `requireAdmin`
  re-checks `is_admin`. Don't swap these for a cache that a revoked user could outlive.
- WebSocket: origin check against `APP_URL`, close codes 1008/1011, `connected`/`ping`/`pong`, and
  every event type that `broadcast` emits.
- DAV: RFC 6764 well-known redirects (308), PROPFIND/REPORT XML, ETag stability, app-password auth.
- Privacy and safety boundaries: BCC never leaks into stored or sent headers it shouldn't be in;
  send idempotency; durable outbox and delivery state; lease and claim ownership; SSRF guard
  (private-host opt-in per account); OIDC state/nonce/PKCE.
- Environment variables and their defaults, so existing `.env` files and compose files keep working.

## 4. Target architecture

### 4.1 Workspace layout

```
backend-rs/
  Cargo.toml                 # workspace, shared profile
  crates/
    inboxora-server/         # binary: axum app, router, startup, shutdown
    core-config/             # env parsing (same names/defaults as Node)
    core-db/                 # sqlx pool, migration runner (same table + checksums), tx helpers
    core-session/            # express-session/connect-redis compatible store + cookie signer
    core-crypto/             # enc:v1 AES-GCM, bcrypt, TOTP, token generation
    core-events/             # Redis pub/sub event bus + WS fan-out
    net-guard/               # SSRF-safe resolver/connector (port of safeFetch + connectionPolicy)
    mail-mime/               # parse (mail-parser), build (mail-builder), body-structure walking
    mail-sanitize/           # HTML sanitizer, differential-tested against sanitize-html
    mail-imap/               # IMAP engine (see §4.3)
    mail-smtp/               # lettre transport + XOAUTH2 + idempotent send
    conversation/            # conversation engine v2 (pure core + persistence)
    calendar/                # iCal + RRULE + projection + CalDAV client/server
    contacts/                # vCard + CardDAV client/server
    auth/                    # password, MFA, OIDC RP, OAuth integrations, device auth
    push/                    # web push (VAPID), FCM, UnifiedPush
    ai/                      # provider adapters, SSE streaming
    plugins/                 # plugin trait + GTD port
    api-*/                   # one crate per route group (thin handlers over the crates above)
  tests/contract/            # black-box suite shared with the Node backend (§7)
```

Use one crate per domain so build times stay manageable, and so the plugin boundary can be
enforced with crate visibility instead of a lint rule.

### 4.2 Crate choices

| Concern | Choice | Notes |
|---|---|---|
| Runtime | `tokio` (multi-thread) | `spawn_blocking`/`rayon` for CPU work (projection, sanitize of huge bodies) |
| HTTP | `axum` + `tower-http` | body limits per route (35 MB send/draft, 8 MB pet import, 1 MB default), CORS, timeouts |
| WebSocket | `axum::extract::ws` (tungstenite) | |
| PostgreSQL | `sqlx` (postgres, rustls, offline mode) | queries checked at compile time against the real schema; statement cache on |
| Redis | `fred` | pipelining, pub/sub, reconnects; sessions + rate limits + event bus |
| Sessions | custom on `fred` | exact connect-redis format, TTL = cookie maxAge, `resave:false`, `saveUninitialized:false` behavior |
| TLS | `rustls` + `aws-lc-rs` | faster handshakes than ring; keep per-account "skip verify" semantics explicit |
| HTTP client | `reqwest` (rustls) behind `net-guard` | DNS resolved and pinned once, private ranges blocked unless allowed |
| IMAP | `imap-next`/`imap-codec` (protocol) + our engine | see §4.3; `async-imap` is the fallback |
| SMTP | `lettre` (tokio, rustls) | XOAUTH2, separate SMTP auth (migration 0040) |
| MIME | `mail-parser`, `mail-builder` | both zero-copy and fuzz-tested (Stalwart) |
| HTML sanitize | `ammonia` (html5ever), with `lol_html` for streaming rewrites | policies must match `emailSanitizer.ts`; differential corpus (§7.3) |
| iCal / vCard | `calcard` | RRULE expansion via `rrule` crate; equivalence-tested against `ical.js` |
| DAV XML | `quick-xml` | |
| OIDC / JWT | `openidconnect`, `jsonwebtoken`/`josekit` | |
| TOTP | `totp-rs` | otplib-compatible defaults (SHA1, 6 digits, 30 s, window) |
| Passwords | `bcrypt` | verify existing `$2a$` hashes; same cost on new hashes |
| Crypto | `aes-gcm` or `aws-lc-rs` AEAD | `enc:v1` format |
| Zip export | `async_zip` | streaming |
| QR | `qrcode` | |
| Rate limiting | `governor` (memory) + Redis keys compatible with rate-limiter-flexible where shared | |
| Observability | `tracing`, `tracing-subscriber` (JSON), histogram metrics matching `performanceMetrics.ts` buckets | |
| Errors | `thiserror` in libraries, one `AppError → Response` mapping | no panics on the request path |

### 4.3 The IMAP engine (highest risk)

The Rust IMAP ecosystem has nothing like `imapflow`. The engine needs IDLE, CONDSTORE (and
QRESYNC where available), UIDPLUS, MOVE, SPECIAL-USE, NAMESPACE, COMPRESS=DEFLATE, XOAUTH2/
OAUTHBEARER, literal+ and robust parsing of non-compliant servers (Exchange, Gmail, Dovecot,
iCloud, GMX…).

Plan:

1. Use `imap-codec`/`imap-next` for wire-level parsing and serialization. They're sans-IO,
   fuzzed, and cover the extensions. Build a small async client on top: tagged command pipeline,
   untagged-response routing, IDLE state machine, timeouts.
2. Port the engine's **policy** code first, as pure functions with the existing unit tests
   translated: `planModseqSync`, `connectCooldownMs`, `persistentEligible`, `classifyMoveBySearch`,
   `walkStructure`, `shouldFallbackToTextPart`, `providerProfile`, `effectiveSyncIntervalMs`, etc.
3. Port the connection manager and sync loops: per-host keyed semaphores, persistent cap,
   background pool size 2, stagger, cooldowns, quiet window, staleness checks, flag-push
   reconciliation and the snippet indexer.
4. **Account ownership lease.** Before either backend syncs an account, it must hold a lease
   (a new additive table, or `pg_try_advisory_lock` on a per-account key held on a dedicated
   connection). This stops Node and Rust syncing the same account during coexistence or a
   rolling deploy. It's also an improvement on its own: today, two Node replicas would both sync.
5. Build a recorded-session test harness: capture IMAP transcripts from the existing Node engine
   against real providers (with credentials scrubbed) and replay them against the Rust client.

### 4.4 Plugins

- Tier-1 (GTD) becomes a Rust crate that depends **only** on a `plugin-api` crate exposing the
  same capabilities as `src/plugins/api.ts`. The compiler then enforces the boundary: a plugin
  crate can't name `core-db` or `mail-imap` because they aren't its dependencies. Add a
  `cargo deny`/`cargo metadata` CI check that the plugin crates' dependency list stays fixed.
- Tier-2 (untrusted, third-party) plugins aren't loadable today. If they're ever needed, run them
  as WASM components (`wasmtime`) against a WIT version of `plugin-api`. This is out of scope for
  parity.

### 4.5 Performance work

The backend spends most of its time waiting on IMAP servers, PostgreSQL and SMTP. Rust doesn't
make those faster. The gains will come from lower per-connection memory, no GC pauses, cheaper
parsing and sanitizing, and parallel CPU work. Apply tweaks in tiers, and adopt a tier-2/3 tweak
only when a benchmark (§8) shows it helps.

**Tier 1: always on.**

- Release profile: `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`, `strip = true`,
  `debug = "line-tables-only"` (symbolized profiles without much size cost). Keep
  `panic = "unwind"` so a panicking task becomes a 500, not a dead process with dropped IMAP
  sessions.
- Global allocator: `mimalloc` (or `tikv-jemallocator` if fragmentation shows up under long IMAP
  uptimes). Benchmark both.
- `sqlx` statement cache, pool sized from measurements (Node uses 20), `statement_timeout`
  applied in startup options as today, and batched or `UNNEST`-based multi-row upserts for sync.
- Stream attachments and raw messages end to end (`Bytes`, `Body::from_stream`). Never buffer a
  25 MB attachment twice.
- Use the `Bytes`/`&str` zero-copy APIs of `mail-parser` and `imap-codec`. Don't build a
  `String` per header.
- `serde` with borrowed `&'a str` fields on hot read paths, and `sonic-rs` or `simd-json` only
  where profiling shows JSON on top (large message lists).
- Small-string and arena types where profiling justifies them: `compact_str` for flags and folder
  names, `smallvec` for flag sets.
- Long-lived IMAP connections: one task per connection, bounded channels, and no per-connection
  OS thread. Target well under the Node per-connection RSS.
- CPU-bound work (calendar projection, sanitize of very large HTML, conversation rebuild) goes to
  `rayon` via `spawn_blocking`, so request-path latency stays flat.
- Compression stays in nginx, not the app.

**Tier 2: adopt if measured.**

- Profile-guided optimization (`cargo pgo`) trained on the contract suite plus a sync replay,
  then BOLT. That's usually 10–20% on parse-heavy code.
- `target-cpu`: published multi-arch images must stay portable. Use `x86-64-v2` as the baseline
  and add a `-v3` image variant only if the gain justifies maintaining it.
- `tokio` tuning: `event_interval`, `global_queue_interval`, worker count, and a dedicated
  runtime for IMAP versus HTTP if one starves the other.
- Redis pipelining for session read plus rate-limit check in one round trip.
- PostgreSQL pipelining (`tokio-postgres` directly) for the hottest multi-query handlers, if
  `sqlx` round trips dominate.
- Prepared-statement-friendly SQL (stable text, no inlined literals) to raise cache hit rates.

**Tier 3: only with strong evidence.**

- `io_uring` (`tokio-uring`/`monoio`): this adds complexity and a Linux-only path, and the
  workload is network-latency-bound. It's unlikely to pay off.
- Custom SIMD in the sanitizer or parser.

**Explicitly not done for performance:** caching the `requireAuth` user check, relaxing
transactions/leases, or skipping fsync-backed durable writes. Those are correctness and security
properties.

## 5. Running both backends side by side

```
            nginx (existing)
   ┌────────────┼──────────────────────┐
   │ paths owned by Node   │ paths owned by Rust │   ← moved one group at a time
   ▼                       ▼
 Node backend          Rust backend
   │   ▲                   │   ▲
   │   └── Redis pub/sub ──┘   │      events: mail.*, calendar.*, sections.*, …
   ├────── Redis sessions ─────┤      same sess:<id> format, same SESSION_SECRET
   ├────── PostgreSQL ─────────┤      Node runs migrations; Rust verifies schema version
   └── IMAP lease table ───────┘      exactly one owner per account
```

- **Migrations:** while both backends run, **only Node** runs migrations. Rust embeds the same
  `migrations/` directory, checks on startup that `schema_migrations` contains every file with a
  matching sha256, and refuses to start otherwise. At final cutover, Rust's runner takes over and
  uses the identical table, advisory lock id (`7418291834`), `-- no-transaction` handling and
  accepted-replaced-checksum list.
- **Events:** Phase 0 adds a Redis pub/sub bus to Node (publish in `broadcast`, and the WS server
  subscribes). Whichever backend owns `/ws` delivers events, wherever they were produced.
- **Mail actions from Rust before the IMAP engine moves:** an internal, authenticated, loopback-only
  RPC from Rust to Node for engine operations. A side effect that fails or times out is reported
  as *uncertain* and never retried automatically as if it were safe (AGENTS.md).
- **Rollback:** each phase ends with "flip this nginx location back to Node". This is only
  possible because neither backend writes data the other can't read. Any new column or table a
  Rust phase needs is added as a Node migration first.

## 6. Phases

Each phase has exit criteria. A phase ships only after its surface passes the parity gates (§7)
and has run in production behind the nginx flip with error rates and latency at or better than
Node. Effort figures are rough single-engineer estimates.

**Phase 0: groundwork (Node + new Rust skeleton). ~2–3 weeks**

- Measure the baseline (§8) and commit the benchmark scripts.
- Node: Redis pub/sub event bus; IMAP account ownership lease (additive migration); internal RPC
  endpoint for engine operations (loopback plus shared secret).
- Rust: workspace, config, `core-db` schema verifier, `core-session` (read and write
  connect-redis sessions; cross-tested so Node reads Rust sessions and vice versa), `core-crypto`
  (`enc:v1` round-trip with Node fixtures), `/api/health`, `/api/version`, metrics, CI job
  (fmt, clippy `-D warnings`, test, `cargo deny`, `sqlx prepare --check`).
- Black-box contract suite scaffolded (§7.1) and green against Node.
- Exit: Rust serves `/api/health` and `/api/version` in a staging deployment; session interop tests
  pass in both directions.

**Phase 1: pure libraries, differential-tested. ~4–6 weeks**

- `mail-mime`, `mail-sanitize`, vCard, iCal + recurrence, `normalizeMessageId`, threading and
  conversation engine pure core, `contentDisposition`, `inlineImages`, `richText`, `composeFormat`.
- Exit: differential corpora (§7.3) show zero unexplained diffs. Each explained diff is listed
  and accepted. Optionally ship the sanitizer/parser to Node through `napi-rs` to get the speedup
  early, behind a flag.

**Phase 2: database-only route groups. ~4–6 weeks**

- Contacts (non-DAV), categories, block list, rules CRUD (not `/api/rules/run`), search, calendar
  read/write (the projection pool moves to `rayon`), push device registry, diagnostics, plugins
  listing/activation, GTD plugin routes that need no engine.
- Exit: contract suite green on Rust for these paths, then flipped in nginx one location at a time.

**Phase 3: DAV servers and external DAV sync. ~3–4 weeks**

- `/carddav`, `/caldav`, well-known redirects, app passwords, CardDAV/CalDAV client schedulers
  (also lease-protected), secret calendar feeds.
- Exit: interoperability runs with DAVx⁵, Thunderbird, Apple Calendar/Contacts and Evolution
  against staging, plus `nginx.dav.test.ts` expectations.

**Phase 4: authentication. ~3–5 weeks**

- Password login, bcrypt, MFA/TOTP and device trust, lock PIN, password reset, OIDC RP (incl.
  RP-initiated logout, admin group claim, login match claim, mail passthrough), OAuth
  integrations, admin.
- Exit: security review of the Rust auth crate; contract suite covers each session-state
  transition (`pendingUserId`, `pendingTOTP*`, `oidcPending`, `locked`); auth events are
  identical.

**Phase 5: mail engine, mail routes, send, WebSocket. ~10–16 weeks**

- IMAP engine (§4.3), `/api/mail/*`, conversations, send/draft (idempotency fingerprints
  byte-compatible), rule forwarder, invitation outbox, snooze watcher, ingest retry, `/ws`.
- Rolled out **per account** through the ownership lease: an admin setting or allow-list moves
  account ownership to Rust. Node stops syncing an account the moment it loses the lease.
- Exit: replayed transcripts green for every provider profile; a week of shadow sync on
  test accounts with no drift (compared per folder: message count, uid/modseq, flags);
  restart/recovery tests for leases, outbox and flag-push reconciliation.

**Phase 6: AI, Todoist, remaining integrations, and Node removal. ~2–4 weeks**

- AI providers (incl. Codex OAuth, SSE), Todoist, sender favicons, any remaining routes.
- Rust takes over migrations; remove `backend/` from images and compose; update the Dockerfile
  to a `cargo-chef` multi-stage build with a distroless or `alpine` runtime.
- Exit: no nginx location points at Node; release notes document the cutover.

**Total: roughly 8–11 engineer-months**, dominated by Phase 5. Two people in parallel (one on
Phase 1–3 libraries and routes, one on the IMAP client from Phase 0) shorten that a lot.

## 7. Parity testing

The existing vitest suites test Node internals with mocks, so they can't be reused as-is.
Parity rests on four layers:

1. **Black-box contract suite** (`backend-rs/tests/contract`, runnable against either backend
   with `BASE_URL`). It runs in CI against docker-compose with PostgreSQL, Redis, Dovecot or
   GreenMail (IMAP), Mailpit (SMTP) and Radicale (DAV). It asserts status, JSON shape
   (schema-snapshotted), headers, and resulting DB/IMAP/SMTP state. It's written first and must be
   green on Node before a Rust surface is compared against it.
2. **Ported unit tests.** Every vitest case in a pure module is translated into a Rust test
   alongside the code. Regression tests named after past fixes (e.g. `send.reliability`,
   `conversationRace`, `conversationTenantSafety`) are ported deliberately, not dropped.
3. **Differential corpora** for pure functions: run Node and Rust on the same inputs and diff the
   outputs. Corpora: the `backend/fixtures` messages, a scrubbed real-mail sample, hostile HTML
   (XSS/mXSS payload lists), malformed MIME, and iCal/vCard files from major providers. Add
   `cargo fuzz` targets for the parser, sanitizer, IMAP codec glue and DAV XML.
4. **Read-only shadow traffic** for GET endpoints in staging: duplicate the request to Rust and
   diff the responses. **Never shadow mutations.** They have external side effects (SMTP, IMAP,
   push) that can't be safely duplicated.

Plus, as AGENTS.md requires: migration-integrity checks; restart and lease recovery tests against
real PostgreSQL and Redis; and `git diff --check`, fmt, clippy and tests before every commit.

## 8. Baseline and performance targets

Measure first, using what's already there (`performanceMetrics.ts` histograms, `diagnosticsReport`,
`benchmarks/calendar-projection-benchmark.mjs`, `scripts/benchmarkMessageList.ts`). Add:

- per-route p50/p95/p99 under load (`oha` or `k6`) for message list, message body, conversation
  list, search, calendar range, contact search, login;
- RSS at idle and with N persistent IMAP connections (N = 10, 100, 500);
- initial backfill throughput (messages/second) and steady-state sync CPU per account;
- sanitize and parse throughput on the fixture corpus;
- cold start to healthy.

Working targets, to confirm or revise after the baseline:

| Metric | Target vs Node |
|---|---|
| RSS per persistent IMAP connection | ≤ 25% |
| Idle RSS | ≤ 20% |
| p95 on CPU-heavy routes (message body, conversation list) | ≤ 50% |
| p95 on DB-bound routes | no regression (bounded by PostgreSQL) |
| Sanitize + parse throughput | ≥ 5× |
| Calendar projection benchmark | ≥ 3× |
| Cold start | ≤ 1 s excluding migrations |

## 9. Risks

| Risk | Mitigation |
|---|---|
| IMAP client maturity / provider quirks | start in Phase 0 in parallel; transcript replay; per-account rollout via lease; Node fallback per account |
| Sanitizer behavior drift (security) | differential corpus + mXSS lists; security review before flip; reject on any unexplained diff |
| Session format mismatch logs users out or, worse, accepts forged cookies | bidirectional interop tests; signature-verification tests with tampered cookies |
| Hash/fingerprint drift (ETags, send idempotency, calendar UIDs) | golden-value tests from Node for every derived identifier; canonical JSON serializer that matches `JSON.stringify` key order |
| Two backends writing the same account | ownership lease is a hard precondition for Phase 5 |
| Feature freeze pressure while porting | port in thin slices; every Node change to a surface that's been ported gets a matching Rust change + contract test in the same PR |
| Scope of Phase 5 | treat it as its own project with weekly drift reports |

## 10. Decisions needed before Phase 0

1. **Repository and branch.** AGENTS.md names `Dragonk/Inboxora` and the `dev` branch, but this
   checkout's `origin` is `ftagsold/Inboxora` and work here is based on `main` (`cb10bf12`), while
   `origin/dev` is at `d7f514c3`. Confirm where the Rust work lands.
2. **Directory:** `backend-rs/` beside `backend/` (recommended), or a separate repository.
3. **MSRV and edition:** recommend stable Rust, edition 2024, MSRV pinned in `rust-toolchain.toml`.
4. **Tier-2 plugins:** confirm they are out of scope for parity (recommended).
5. **Early wins:** whether Phase 1 libraries should first ship into Node through `napi-rs`.
6. **Release versioning:** the cutover is a major operational change. Choose the target version
   before Phase 6 changes the release notes and changelog (AGENTS.md: don't infer versions).
