# syntax=docker/dockerfile:1

# CSS build stage - download and run standalone tailwindcss
FROM debian:trixie-slim AS css-builder

# Declare TARGETARCH to receive automatic value from BuildKit
ARG TARGETARCH

WORKDIR /app

# Download standalone tailwindcss CLI with checksum verification
# Checksums for v4.3.2:
#   tailwindcss-linux-x64:   5036c4fb4328e0bcdbb6065c70d8ac9452e0d4c947113a788a8f94fd390425c1
#   tailwindcss-linux-arm64: 394ddccc2402cfa3abd97dfba56f3587781a3d6e6ce66e65ceada14beb7664b8
RUN apt-get update && apt-get install -y curl \
    && rm -rf /var/lib/apt/lists/* \
    && case "$TARGETARCH" in \
         amd64) \
           BINARY="tailwindcss-linux-x64" \
           CHECKSUM="5036c4fb4328e0bcdbb6065c70d8ac9452e0d4c947113a788a8f94fd390425c1" \
           ;; \
         arm64) \
           BINARY="tailwindcss-linux-arm64" \
           CHECKSUM="394ddccc2402cfa3abd97dfba56f3587781a3d6e6ce66e65ceada14beb7664b8" \
           ;; \
         *) \
           echo "Unsupported architecture: $TARGETARCH" && exit 1 \
           ;; \
       esac \
    && curl -sLO "https://github.com/tailwindlabs/tailwindcss/releases/download/v4.3.2/${BINARY}" \
    && echo "${CHECKSUM}  ${BINARY}" | sha256sum -c - \
    && chmod +x "${BINARY}" \
    && mv "${BINARY}" tailwindcss

# Copy CSS build files and templates (needed for content scanning)
COPY crates/devbox-server/static crates/devbox-server/static
COPY crates/devbox-server/tailwind.config.js crates/devbox-server/
COPY crates/devbox-server/styles crates/devbox-server/styles
COPY crates/devbox-server/templates crates/devbox-server/templates
COPY crates/devbox-server/src crates/devbox-server/src

# Build minified CSS
RUN cd crates/devbox-server \
    && /app/tailwindcss -i styles/input.css -o static/css/output.css --minify

# cargo-chef base stage - shared between planner and builder
FROM rust:1.97.0-alpine AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# Planner stage - generate dependency recipe from workspace manifests
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/devbox-common/Cargo.toml crates/devbox-common/
COPY crates/devbox-server/Cargo.toml crates/devbox-server/
COPY crates/devbox-cli/Cargo.toml crates/devbox-cli/

# Create dummy source files so cargo metadata can resolve the workspace
RUN mkdir -p crates/devbox-common/src && touch crates/devbox-common/src/lib.rs \
    && mkdir -p crates/devbox-server/src && touch crates/devbox-server/src/lib.rs crates/devbox-server/src/main.rs \
    && mkdir -p crates/devbox-cli/src && touch crates/devbox-cli/src/main.rs
RUN cargo chef prepare --recipe-path recipe.json

# Rust build stage - using musl for static binary
FROM chef AS builder

# Build argument for reproducible builds
ARG SOURCE_DATE_EPOCH=0

# Install build dependencies for static compilation
# clang is required for aws-lc-rs FIPS delocator on aarch64
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig cmake make go perl clang linux-headers
ENV AWS_LC_FIPS_SYS_CC=clang
ENV AWS_LC_FIPS_SYS_CXX=clang++

# Cook dependencies (cached until Cargo.toml/Cargo.lock change)
COPY --from=planner /app/recipe.json recipe.json
ENV OPENSSL_STATIC=1
ENV OPENSSL_LIB_DIR=/usr/lib
ENV OPENSSL_INCLUDE_DIR=/usr/include
RUN cargo chef cook --release --package devbox-server --recipe-path recipe.json

# Restore real manifests (cook leaves stubs with placeholder versions)
COPY Cargo.toml Cargo.lock ./
COPY crates/devbox-common/Cargo.toml crates/devbox-common/
COPY crates/devbox-server/Cargo.toml crates/devbox-server/
COPY crates/devbox-cli/Cargo.toml crates/devbox-cli/

# Copy actual source code
COPY crates/devbox-common/src crates/devbox-common/src
COPY crates/devbox-server/src crates/devbox-server/src
COPY crates/devbox-server/migrations crates/devbox-server/migrations
COPY crates/devbox-server/templates crates/devbox-server/templates

# Copy built static assets (needed at compile time for rust-embed)
COPY --from=css-builder /app/crates/devbox-server/static crates/devbox-server/static

# Touch files with deterministic timestamp to ensure rebuild
RUN touch -d "@${SOURCE_DATE_EPOCH}" crates/devbox-common/src/lib.rs crates/devbox-server/src/main.rs

# Build the release binary with static linking
RUN cargo build --release --package devbox-server

# Create empty data directory marker
RUN mkdir -p /data && touch /data/.keep

# Runtime stage - minimal static distroless image (no glibc)
FROM gcr.io/distroless/static-debian13:nonroot

WORKDIR /

LABEL org.opencontainers.image.source=https://github.com/smoketurner/devbox

# Copy the binary (static assets are embedded via rust-embed)
COPY --from=builder /app/target/release/devbox-server /devbox-server

# Create data directory with correct ownership
COPY --from=builder --chown=nonroot:nonroot /data /data

# Environment defaults
ENV LISTEN_ADDR=0.0.0.0:3000
ENV DATABASE_URL=sqlite:/data/devbox.db?mode=rwc

EXPOSE 3000

ENTRYPOINT ["/devbox-server"]
