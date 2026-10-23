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

# The Rust stages run on the build machine's own architecture and cross-compile for the target
# (rustc crashes under QEMU emulation); only the distroless runtime stage is per-target.
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim-bookworm AS chef
ARG BUILDARCH
RUN cargo install cargo-chef --locked --version 0.1.73 \
    && case "$BUILDARCH" in \
         arm64) cross="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
         *) cross="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
       esac \
    && apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev $cross \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
WORKDIR /src

FROM chef AS planner
COPY backend/ ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG BUILD_SHA
ARG BUILDARCH
ARG TARGETARCH
# The target triple, and a cross C toolchain for crates with C parts (ring) when the target
# differs from the build machine.
RUN case "$TARGETARCH" in amd64) t=x86_64-unknown-linux-gnu ;; arm64) t=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; esac \
    && echo "$t" > /target-triple \
    && if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
         gcc=$(echo "$t" | sed 's/-unknown//')-gcc; \
         u=$(echo "$t" | tr 'a-z-' 'A-Z_'); l=$(echo "$t" | tr '-' '_'); \
         printf 'export CARGO_TARGET_%s_LINKER=%s\nexport CC_%s=%s\n' "$u" "$gcc" "$l" "$gcc" > /cross-env; \
       else : > /cross-env; fi
COPY --from=planner /src/recipe.json recipe.json
RUN . /cross-env && cargo chef cook --release --locked -p inboxora-server --target "$(cat /target-triple)" --recipe-path recipe.json
COPY backend/ ./
RUN . /cross-env && t=$(cat /target-triple) \
    && cargo build --release --locked -p inboxora-server --target "$t" \
    && cp "target/$t/release/inboxora-server" /inboxora-server \
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
