# Builder stage
FROM rust:1-trixie@sha256:1bcff4befb740599103a2c7cb51058e14479b2e35e3a34a3f0dc4ede09927488 AS builder

WORKDIR /app

# Copy source code
COPY . .

# Build the release binary and prepare the runtime cache mount point.
RUN cargo build --release && mkdir -p /app/cache

# Runtime stage
FROM gcr.io/distroless/cc-debian13@sha256:ed7c407fd64eb0af9dddb9456b94cee188a40a7f53cf38c9836e1e9ae14fca02

WORKDIR /app

# Run as distroless nonroot and keep Zebra's peer cache out of /root.
ENV XDG_CACHE_HOME=/cache \
    ZEEDER__DNS__LISTEN_ADDR=0.0.0.0:1053

COPY --from=builder --chown=65532:65532 /app/target/release/zeeder /app/zeeder
COPY --from=builder --chown=65532:65532 /app/cache /cache

# 1053: DNS (UDP/TCP)
# 9999: Metrics (TCP)
# 8080: Health and readiness (TCP)
EXPOSE 1053/udp 1053/tcp 9999/tcp 8080/tcp

USER 65532:65532

ENTRYPOINT ["/app/zeeder", "start"]
