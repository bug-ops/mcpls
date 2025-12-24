# Multi-stage build for mcpls
# Build stage uses rust:1.85-slim, runtime uses debian:bookworm-slim

# Build stage
FROM rust:1.85-slim as builder

WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build release binary
RUN cargo build --release --package mcpls

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies (CA certificates for HTTPS)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy only the binary from builder stage
COPY --from=builder /app/target/release/mcpls /usr/local/bin/mcpls

# Create config directory
RUN mkdir -p /etc/mcpls

# Set default environment variables
ENV MCPLS_CONFIG=/etc/mcpls/mcpls.toml
ENV MCPLS_LOG=info

# Run as non-root user for security
RUN useradd -m -u 1000 mcpls && \
    chown -R mcpls:mcpls /etc/mcpls

USER mcpls
WORKDIR /home/mcpls

ENTRYPOINT ["mcpls"]
CMD []
