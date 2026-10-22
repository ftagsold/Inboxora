# Inboxora backend (Rust)

Rust rewrite of `backend/`, wire-compatible with the frontend, the Android app, DAV clients and
existing databases. The plan, ordering and parity gates are in
[`docs/rust-backend-migration-plan.md`](../docs/rust-backend-migration-plan.md).

## Layout

| Crate | Contents |
|---|---|
| `crates/core` | config (Node env names/defaults), PostgreSQL pool + node-pg compatible params/rows, migration runner, express-session compatible sessions, `enc:v1` crypto, Redis, event fan-out, metrics |
| `crates/server` | axum binary: Express-equivalent middleware stack, extractors, WebSocket, routes |

## Running

```bash
cargo build --release -p inboxora-server
# Same environment as the Node backend (DB_*, REDIS_URL, SESSION_SECRET, ENCRYPTION_KEY, APP_URL, …).
# MIGRATIONS=verify: only check the schema (while the Node backend owns migrations).
# MIGRATIONS_DIR: defaults to ./migrations, then ../backend/migrations.
./target/release/inboxora-server
```

## Tests

```bash
# Unit tests; integration tests run when TEST_DB_HOST/TEST_REDIS_URL are set (see .env.test).
set -a; . ./.env.test; set +a; cargo test

# Session interop (Node ↔ Rust) — run from backend/ so Node resolves its dependencies:
cargo build --example session_probe
(cd ../backend && cp ../backend-rs/tests/interop/session.mjs .interop.mjs && \
  PROBE=../backend-rs/target/debug/examples/session_probe SESSION_SECRET=$(printf 'k%.0s' {1..40}) node .interop.mjs; rm .interop.mjs)

# Differential contract suite: starts both backends on fresh databases and compares responses.
node tests/contract/harness.mjs [filter]
```

`.env.test` (not committed) holds `TEST_DB_HOST`, `TEST_DB_PORT`, `TEST_DB_PASSWORD`,
`TEST_REDIS_URL`. The contract harness reads `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`,
`REDIS_HOST`, `REDIS_PORT` (defaults: localhost:55432, mailflow/mailflow, 127.0.0.1:56379).

## Porting status

| Phase | Area | Status |
|---|---|---|
| 0 | core, middleware stack, sessions, migrations, WebSocket, health/version, contract harness | done |
| 1 | pure libraries (MIME, sanitizer, vCard, iCal/recurrence, threading) | pending |
| 2 | database-only route groups | pending |
| 3 | DAV servers and DAV sync | pending |
| 4 | authentication | pending |
| 5 | mail engine, mail routes, send, WebSocket events | pending |
| 6 | AI, integrations, cutover | pending |

Plugins (GTD) are out of scope for the rewrite.

## Intentional deviations from the Node backend

- A malformed JSON body returns `400 {"error":"Invalid JSON body"}` (Node returned a generic 500).
- `204` responses carry no `Content-Length` (forbidden by RFC 9110; hyper strips it).
