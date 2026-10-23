# Inboxora: the Rust server and the web app it serves, in one image. Build from the repository
# root:
#   docker build -f backend/Dockerfile -t inboxora .
#
# cargo-chef caches the dependency build in its own layer, so a source-only change rebuilds
# just the workspace crates. The runtime is distroless (glibc, CA certificates, non-root, no
# shell); the healthcheck is the binary's own `healthcheck` subcommand.

ARG RUST_VERSION=1.94.1

FROM --platform=$BUILDPLATFORM node:22-alpine AS web
WORKDIR /app
COPY frontend/package*.json ./
# electron is a devDependency used only for desktop packaging (electron-builder), never by the
# web/vite build. Skip its ~100MB binary download so multi-arch builds stay fast.
ENV ELECTRON_SKIP_BINARY_DOWNLOAD=1
RUN npm ci
COPY frontend/ ./
ARG VITE_BUILD_SHA
ENV VITE_BUILD_SHA=${VITE_BUILD_SHA:-dev}
ARG VITE_EMAIL_DIV_RENDER
ENV VITE_EMAIL_DIV_RENDER=${VITE_EMAIL_DIV_RENDER:-false}
RUN npm run build

FROM rust:${RUST_VERSION}-slim-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.73
WORKDIR /src

FROM chef AS planner
COPY backend/ ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG BUILD_SHA
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked -p inboxora-server --recipe-path recipe.json
COPY backend/ ./
RUN cargo build --release --locked -p inboxora-server \
    && cp target/release/inboxora-server /inboxora-server \
    && mkdir /certs

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
ARG BUILD_SHA
ARG APP_VERSION
# HTTP on 3000, HTTPS on 3443 (certificate from /app/certs, self-signed when absent).
ENV BUILD_SHA=${BUILD_SHA:-dev} \
    APP_VERSION=${APP_VERSION} \
    NODE_ENV=production \
    PORT=3000 \
    TLS_PORT=3443 \
    MIGRATIONS_DIR=/app/migrations \
    STATIC_DIR=/app/public \
    VITE_APP_NAME=Inboxora
WORKDIR /app
COPY --from=build /inboxora-server /app/inboxora-server
COPY backend/migrations /app/migrations
COPY --from=web /app/dist /app/public
# Writable by the nonroot user, so a generated certificate can be stored without a volume.
COPY --from=build --chown=65532:65532 /certs /app/certs
EXPOSE 80 443 3000 3443
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 CMD ["/app/inboxora-server", "healthcheck"]
ENTRYPOINT ["/app/inboxora-server"]
