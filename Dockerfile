# Multi-stage build for mcpls
# Build stage uses rust:1.88-slim, runtime uses debian:bookworm-slim

# Build stage
FROM rust:1.88-slim AS builder

WORKDIR /app

# Cache dependencies separately from source code.
# Copy manifests first, build a dummy main to populate the registry cache,
# then overwrite with real sources. This layer is invalidated only when
# Cargo.toml or Cargo.lock changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/mcpls-core/Cargo.toml ./crates/mcpls-core/
COPY crates/mcpls-cli/Cargo.toml ./crates/mcpls-cli/
RUN mkdir -p crates/mcpls-core/src crates/mcpls-cli/src && \
    echo "pub fn main() {}" > crates/mcpls-core/src/lib.rs && \
    echo "fn main() {}" > crates/mcpls-cli/src/main.rs && \
    cargo build --release --package mcpls && \
    rm -rf crates/mcpls-core/src crates/mcpls-cli/src

# Now copy real sources and rebuild only the changed crates
COPY crates/ ./crates/
RUN cargo build --release --package mcpls

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/mcpls /usr/local/bin/mcpls

RUN mkdir -p /etc/mcpls && \
    useradd -m -u 1000 mcpls && \
    chown mcpls:mcpls /etc/mcpls

ENV MCPLS_CONFIG=/etc/mcpls/mcpls.toml
ENV MCPLS_LOG=info

USER mcpls
WORKDIR /home/mcpls

ENTRYPOINT ["mcpls"]
