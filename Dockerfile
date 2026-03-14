# Stage 1: Build
FROM rust:1.85-slim AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    cmake \
    g++ \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache dependency builds: copy manifests first
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null || true

# Copy real source
COPY src/ src/
COPY benches/ benches/
COPY janus.toml ./

# Touch main.rs so cargo rebuilds with real source
RUN touch src/main.rs

RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/janus /usr/local/bin/janus
COPY --from=builder /app/janus.toml /etc/janus/janus.toml
COPY --from=builder /app/benches/ /app/benches/

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s \
    CMD curl -f http://localhost:8080/health || exit 1

ENTRYPOINT ["janus"]
CMD ["serve", "--config", "/etc/janus/janus.toml", "--no-tui"]
