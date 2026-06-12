# Builder stage
FROM rust:1-trixie AS builder

WORKDIR /app

# Copy source code
COPY . .

# Build the release binary and prepare the runtime cache mount point.
RUN cargo build --release && mkdir -p /app/cache

# Runtime stage
FROM gcr.io/distroless/cc-debian13

WORKDIR /app

# Run as distroless nonroot and keep Zebra's peer cache out of /root.
ENV XDG_CACHE_HOME=/cache \
    ZEEDER__DNS__LISTEN_ADDR=0.0.0.0:1053

COPY --from=builder --chown=65532:65532 /app/target/release/zeeder /app/zeeder
COPY --from=builder --chown=65532:65532 /app/cache /cache

# 1053: DNS (UDP/TCP)
# 9999: Metrics (TCP)
EXPOSE 1053/udp 1053/tcp 9999/tcp

USER 65532:65532

ENTRYPOINT ["/app/zeeder", "start"]
