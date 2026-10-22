# Inboxora backend (Rust). Build from the repository root:
#   docker build -f backend-rs/Dockerfile -t inboxora-backend .
#
# cargo-chef caches the dependency build in its own layer, so a source-only change rebuilds
# just the workspace crates. The runtime is distroless (glibc, CA certificates, non-root, no
# shell); the healthcheck is the binary's own `healthcheck` subcommand.

ARG RUST_VERSION=1.94.1

FROM rust:${RUST_VERSION}-slim-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.73
WORKDIR /src

FROM chef AS planner
COPY backend-rs/ ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG BUILD_SHA
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked -p inboxora-server --recipe-path recipe.json
COPY backend-rs/ ./
RUN cargo build --release --locked -p inboxora-server \
 && cp target/release/inboxora-server /inboxora-server

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
ARG BUILD_SHA
ARG APP_VERSION
ENV BUILD_SHA=${BUILD_SHA:-dev} \
    APP_VERSION=${APP_VERSION} \
    NODE_ENV=production \
    PORT=3000 \
    MIGRATIONS_DIR=/app/migrations
WORKDIR /app
COPY --from=build /inboxora-server /app/inboxora-server
COPY backend/migrations /app/migrations
EXPOSE 3000
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 CMD ["/app/inboxora-server", "healthcheck"]
ENTRYPOINT ["/app/inboxora-server"]
