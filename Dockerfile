# ── Build stage ──────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

# rusqlite's "bundled" feature compiles SQLite from C source.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies separately from source so they aren't rebuilt on every
# source change.  Build a dummy binary first, then overwrite with the real one.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch main.rs so Cargo notices the source changed.
RUN touch src/main.rs && cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

COPY --from=builder /build/target/release/servito /usr/local/bin/servito

# /config  – mount a volume or bind-mount containing config.toml
# /music   – bind-mount your MP3 library here (read-only is fine)
# /data    – writable volume for the SQLite database
VOLUME ["/config", "/music", "/data"]

# Default stream port — must match [stream] port in your config.toml
EXPOSE 8000

STOPSIGNAL SIGKILL

ENTRYPOINT ["servito", "-c", "/config/config.toml"]
# Default command: run the stream server.
# Override with "library scan" or "library list" etc. for one-off commands.
CMD ["stream"]
