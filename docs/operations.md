# Operations Guide

Complete guide for configuring, deploying, and operating zeeder.

## Configuration

### Configuration Sources

Configuration is loaded in priority order:
1. **Environment variables** (`ZEEDER__*`) - highest priority
2. **TOML config file** - medium priority
3. **Hardcoded defaults** - lowest priority

### Environment Variables

Prefix all variables with `ZEEDER__` and use double underscores for nesting. Zeeder does not use Zebra's `ZEBRA_` namespace, so colocated `zebrad` processes can keep their own `ZEBRA_*` configuration without reading Zeeder settings.

```bash
# Core settings
ZEEDER__DNS__LISTEN_ADDR="0.0.0.0:53"
ZEEDER__DNS__DOMAIN="mainnet.seeder.example.com"
ZEEDER__DNS__NAMESERVER="ns.seeder.example.com"
ZEEDER__DNS__TTL="600"

# Crawler
ZEEDER__CRAWLER__NETWORK="Mainnet"  # or "Testnet"

# Rate limiting (recommended for production)
ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND="10"
ZEEDER__RATE_LIMIT__BURST_SIZE="20"

# Metrics (optional)
ZEEDER__METRICS__ENDPOINT_ADDR="0.0.0.0:9999"
```

### `.env` File

Create `.env` in project root (see [`.env.example`](../.env.example)):

```bash
cp .env.example .env
# Edit .env with your values
```

The `.env` file is optional. If it exists, it must parse successfully or the
seeder exits before loading configuration.

### TOML Config File

Example `config.toml`:

```toml
[dns]
listen_addr = "0.0.0.0:53"
domain = "mainnet.seeder.example.com"
nameserver = "ns.seeder.example.com"
ttl = 600

[crawler]
network = "Mainnet"

[rate_limit]
queries_per_second = 10
burst_size = 20

[metrics]
endpoint_addr = "0.0.0.0:9999"
```

Use with: `zeeder start --config config.toml`

### Configuration Reference

| Parameter | Environment Variable | Default | Description |
|-----------|---------------------|---------|-------------|
| `dns.listen_addr` | `ZEEDER__DNS__LISTEN_ADDR` | `0.0.0.0:53` | DNS server address and port |
| `dns.domain` | `ZEEDER__DNS__DOMAIN` | `mainnet.seeder.example.com` | Authoritative domain |
| `dns.nameserver` | `ZEEDER__DNS__NAMESERVER` | `ns.seeder.example.com` | Out-of-zone authoritative nameserver |
| `dns.ttl` | `ZEEDER__DNS__TTL` | `600` | DNS response TTL in seconds |
| `crawler.network` | `ZEEDER__CRAWLER__NETWORK` | `Mainnet` | Zcash network (`Mainnet` or `Testnet`) |
| `rate_limit.queries_per_second` | `ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND` | `10` | Max queries/sec per IP; must be greater than 0 |
| `rate_limit.burst_size` | `ZEEDER__RATE_LIMIT__BURST_SIZE` | `20` | Burst capacity; must be greater than 0 |
| `metrics.endpoint_addr` | `ZEEDER__METRICS__ENDPOINT_ADDR` | (disabled) | Prometheus endpoint |

## Deployment

### Prerequisites

- **DNS delegation**: Your configured `dns.domain` must delegate to `dns.nameserver`, and `dns.nameserver` must resolve outside `dns.domain`
- **Port 53**: UDP (and optionally TCP) access required
- **Outbound connectivity**: Access to the Zcash P2P network (port 8233 for mainnet, 18233 for testnet)
- **Crawler listener**: The crawler binds `[::]:8233` on mainnet and `[::]:18233` on testnet. Expose that listener only if you want the seeder to accept inbound P2P connections.
- **Resources**: ~100MB RAM, minimal CPU

### Docker Deployment (Recommended)

**1. Build image:**
```bash
docker build -t zeeder .
```

**2. Run with docker-compose:**
```yaml
version: "3.8"
services:
  seeder:
    image: zeeder
    restart: unless-stopped
    ports:
      - "53:1053/udp"
      - "53:1053/tcp"
      - "9999:9999/tcp"  # metrics
    environment:
      ZEEDER__DNS__DOMAIN: "mainnet.seeder.example.com"
      ZEEDER__DNS__NAMESERVER: "ns.seeder.example.com"
      ZEEDER__CRAWLER__NETWORK: "Mainnet"
      ZEEDER__DNS__LISTEN_ADDR: "0.0.0.0:1053"
      ZEEDER__DNS__TTL: "600"
      ZEEDER__METRICS__ENDPOINT_ADDR: "0.0.0.0:9999"
    volumes:
      - zeeder-cache:/cache

volumes:
  zeeder-cache:
```

**3. Start:**
```bash
docker-compose up -d
```

**4. Verify:**
```bash
dig @localhost mainnet.seeder.example.com A
```

### Bare Metal Deployment

**1. Install Rust:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**2. Build:**
```bash
cd zeeder
cargo build --release
```

**3. Create systemd service** (`/etc/systemd/system/zeeder.service`):
```ini
[Unit]
Description=Zcash DNS Seeder
After=network.target

[Service]
Type=simple
User=zebra
WorkingDirectory=/opt/zeeder
Environment="ZEEDER__DNS__DOMAIN=mainnet.seeder.example.com"
Environment="ZEEDER__DNS__NAMESERVER=ns.seeder.example.com"
Environment="ZEEDER__CRAWLER__NETWORK=Mainnet"
Environment="ZEEDER__METRICS__ENDPOINT_ADDR=0.0.0.0:9999"
ExecStart=/opt/zeeder/target/release/zeeder start
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

**4. Enable and start:**
```bash
sudo systemctl daemon-reload
sudo systemctl enable zeeder
sudo systemctl start zeeder
```

### DNS Setup

**Example DNS zone configuration:**

```bind
; Host the nameserver address outside the served seed domains
seeder.example.com.    IN  NS      ns.seeder.example.com.
ns.seeder.example.com. IN  A       203.0.113.10

; The seeder will answer for these:
mainnet.seeder.example.com. IN NS ns.seeder.example.com.
testnet.seeder.example.com. IN NS ns.seeder.example.com.
```

**Verify delegation:**
```bash
dig mainnet.seeder.example.com NS
dig @203.0.113.10 mainnet.seeder.example.com A
```

### Firewall Configuration

```bash
# Allow DNS queries
ufw allow 53/udp
ufw allow 53/tcp

# Allow metrics (from monitoring network only)
ufw allow from 10.0.0.0/8 to any port 9999 proto tcp

# Optional: allow inbound P2P crawler connections (choose one network)
ufw allow 8233/tcp   # mainnet
ufw allow 18233/tcp  # testnet

# Allow outbound to Zcash network
# (Usually no action needed for outbound)
```

### Security Checklist

- ✅ Rate limiting enabled (`rate_limit` configured)
- ✅ Metrics endpoint firewalled (if exposed)
- ✅ Running as non-root user
- ✅ DNS domain validation (automatic)
- ✅ Regular security updates
- ✅ Monitor `zeeder_mutex_poisoning_total` metric

## Monitoring & Operations

### Metrics

**Metrics endpoint:** `http://localhost:9999/metrics` (if enabled)

### Health Checks

**Liveness:** the DNS server is live when it returns an authoritative response
for the configured seed domain:

```bash
dig @127.0.0.1 -p 1053 testnet.seeder.example.com SOA
```

**Readiness:** the crawler is ready to serve bootstrap peers when at least one
address family has a non-zero servable-peer gauge:

```bash
curl -s http://localhost:9999/metrics | grep 'zeeder_peers_servable'
```

A zero gauge means DNS can still answer zone metadata and NODATA responses, but
bootstrap A/AAAA answers will be empty for that address family.

**Critical Metrics to Monitor:**

| Metric | Type | Labels | Description | Alert If |
|--------|------|--------|-------------|----------|
| `zeeder_peers_servable` | Gauge | `addr_family=v4\|v6` | Servable peers (recently-live, current-version, outbound, clean) | < 10 |
| `zeeder_peers_unservable` | Gauge | `reason=not_routable\|wrong_port\|not_recently_live\|not_full_node\|inbound\|misbehaving` | Unservable peers, by reason | - |
| `zeeder_peers_known` | Gauge | - | Total peers in the address book | - |
| `zeeder_min_protocol_version` | Gauge | - | Enforced protocol-version floor | changes only at a network upgrade |
| `zeeder_build_info` | Gauge | `version`, `git_sha`, `network` | Build and network identification | - |
| `zeeder_mutex_poisoning_total` | Counter | - | Mutex poisoning events | > 0 |
| `zeeder_dns_rate_limited_total` | Counter | - | Rate-limited queries | Spike indicates attack |
| `zeeder_dns_errors_total` | Counter | - | DNS errors | > 0 (sustained) |
| `zeeder_dns_queries_total` | Counter | `record_type=A\|AAAA\|SOA\|NS\|other` | Total queries | - |
| `zeeder_dns_response_peers` | Summary | - | Peers per response | - |

### Sample Prometheus Queries

**Servable peer count:**
```promql
zeeder_peers_servable{addr_family="v4"}
zeeder_peers_servable{addr_family="v6"}
```

**Query rate (queries/sec):**
```promql
rate(zeeder_dns_queries_total[5m])
```

**Rate limiting rate:**
```promql
rate(zeeder_dns_rate_limited_total[5m])
```

**Average peers per response:**
```promql
rate(zeeder_dns_response_peers_sum[5m]) / rate(zeeder_dns_response_peers_count[5m])
```

### Alerting Rules

**Example Prometheus alerts:**

```yaml
groups:
  - name: zeeder
    rules:
      - alert: SeederLowPeerCount
        expr: zeeder_peers_servable < 10
        for: 15m
        annotations:
          summary: "Seeder has low peer count"
          
      - alert: SeederMutexPoisoned
        expr: increase(zeeder_mutex_poisoning_total[5m]) > 0
        annotations:
          summary: "CRITICAL: Mutex poisoning detected"
          
      - alert: SeederHighRateLimiting
        expr: rate(zeeder_dns_rate_limited_total[5m]) > 10
        for: 5m
        annotations:
          summary: "High rate limiting (possible attack)"
```

### Troubleshooting

**No peers returning:**
```bash
# Check peer count
curl -s http://localhost:9999/metrics | grep 'zeeder_peers_servable'

# Check logs for errors
journalctl -u zeeder -n 100

# Verify network connectivity
dig @seed.electriccoin.co mainnet.z.cash A
```

**DNS not responding:**
```bash
# Verify server is listening
ss -ulnp | grep :53

# Check logs
journalctl -u zeeder -f

# Test locally
dig @127.0.0.1 -p 1053 testnet.seeder.example.com A
```

**Rate limiting too aggressive:**
```bash
# Increase limits (adjust for your traffic)
export ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND="20"
export ZEEDER__RATE_LIMIT__BURST_SIZE="40"

# Restart seeder
systemctl restart zeeder
```

**High memory usage:**
```bash
# Check address book size
curl -s http://localhost:9999/metrics | grep 'zeeder_peers_known'

# Clear cache if needed (will rebuild)
rm -rf ~/.cache/zebra/network/*
systemctl restart zeeder
```

### Maintenance

**Viewing logs:**
```bash
# Systemd
journalctl -u zeeder -f

# Docker
docker-compose logs -f seeder
```

**Restarting:**
```bash
# Systemd
systemctl restart zeeder

# Docker
docker-compose restart seeder
```

**Upgrading:**
```bash
# Systemd
sudo systemctl stop zeeder
cd /opt/zeeder
git pull
cargo build --release
sudo systemctl start zeeder

# Docker
docker-compose pull
docker-compose up -d
```

**Cache persistence:**
- Address book cached in `~/.cache/zebra/network/` by default
- The Docker image sets `XDG_CACHE_HOME=/cache`, so its peer cache lives under `/cache/zebra/network/`
- Persisting this directory speeds up startup
- Safe to delete (will rebuild from network)

### Capacity Planning

**Expected resource usage:**
- **RAM**: ~50-100MB
- **CPU**: <5% (single core)
- **Disk**: <10MB (cache)
- **Network**: ~1-10 Mbps depending on query volume

**Scaling:**
- Single instance handles 1000s of queries/sec
- Scale horizontally with DNS round-robin if needed
- Rate limiting per-IP prevents single-source overload
