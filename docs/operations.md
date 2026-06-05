# Operations Guide

Complete guide for configuring, deploying, and operating zebra-seeder.

## Configuration

### Configuration Sources

Configuration is loaded in priority order:
1. **Environment variables** (`ZEBRA_SEEDER__*`) - highest priority
2. **TOML config file** - medium priority
3. **Hardcoded defaults** - lowest priority

### Environment Variables

Prefix all variables with `ZEBRA_SEEDER__` and use double underscores for nesting:

```bash
# Core settings
ZEBRA_SEEDER__DNS_LISTEN_ADDR="0.0.0.0:53"
ZEBRA_SEEDER__SEED_DOMAIN="mainnet.seeder.example.com"
ZEBRA_SEEDER__DNS_TTL="600"

# Network
ZEBRA_SEEDER__NETWORK__NETWORK="Mainnet"  # or "Testnet"

# Rate limiting (recommended for production)
ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND="10"
ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE="20"

# Metrics (optional)
ZEBRA_SEEDER__METRICS__ENDPOINT_ADDR="0.0.0.0:9999"
```

### `.env` File

Create `.env` in project root (see [`.env-example.txt`](../.env-example.txt)):

```bash
cp .env-example.txt .env
# Edit .env with your values
```

### TOML Config File

Example `config.toml`:

```toml
dns_listen_addr = "0.0.0.0:53"
seed_domain = "mainnet.seeder.example.com"
dns_ttl = 600

[network]
network = "Mainnet"

[rate_limit]
queries_per_second = 10
burst_size = 20

[metrics]
endpoint_addr = "0.0.0.0:9999"
```

Use with: `zebra-seeder start --config config.toml`

### Configuration Reference

| Parameter | Environment Variable | Default | Description |
|-----------|---------------------|---------|-------------|
| `dns_listen_addr` | `ZEBRA_SEEDER__DNS_LISTEN_ADDR` | `0.0.0.0:53` | DNS server address and port |
| `dns_ttl` | `ZEBRA_SEEDER__DNS_TTL` | `600` | DNS response TTL in seconds |
| `seed_domain` | `ZEBRA_SEEDER__SEED_DOMAIN` | `mainnet.seeder.example.com` | Authoritative domain |
| `network.network` | `ZEBRA_SEEDER__NETWORK__NETWORK` | `Mainnet` | Zcash network (`Mainnet` or `Testnet`) |
| `rate_limit.queries_per_second` | `ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND` | `10` | Max queries/sec per IP |
| `rate_limit.burst_size` | `ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE` | `20` | Burst capacity |
| `metrics.endpoint_addr` | `ZEBRA_SEEDER__METRICS__ENDPOINT_ADDR` | (disabled) | Prometheus endpoint |

## Deployment

### Prerequisites

- **DNS delegation**: Your `seed_domain` must have NS records pointing to your server
- **Port 53**: UDP (and optionally TCP) access required
- **Outbound connectivity**: Access to Zcash P2P network (port 8233 for mainnet, 18233 for testnet)
- **Resources**: ~100MB RAM, minimal CPU

### Docker Deployment (Recommended)

**1. Build image:**
```bash
docker build -t zebra-seeder .
```

**2. Run with docker-compose:**
```yaml
version: "3.8"
services:
  seeder:
    image: zebra-seeder
    restart: unless-stopped
    ports:
      - "53:53/udp"
      - "9999:9999"  # metrics
    environment:
      ZEBRA_SEEDER__SEED_DOMAIN: "mainnet.seeder.example.com"
      ZEBRA_SEEDER__NETWORK__NETWORK: "Mainnet"
      ZEBRA_SEEDER__DNS_TTL: "600"
      ZEBRA_SEEDER__METRICS__ENDPOINT_ADDR: "0.0.0.0:9999"
    volumes:
      - seeder-cache:/root/.cache/zebra/network  # Persist address book

volumes:
  seeder-cache:
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
cd zebra-seeder
cargo build --release
```

**3. Create systemd service** (`/etc/systemd/system/zebra-seeder.service`):
```ini
[Unit]
Description=Zcash DNS Seeder
After=network.target

[Service]
Type=simple
User=zebra
WorkingDirectory=/opt/zebra-seeder
Environment="ZEBRA_SEEDER__SEED_DOMAIN=mainnet.seeder.example.com"
Environment="ZEBRA_SEEDER__NETWORK__NETWORK=Mainnet"
Environment="ZEBRA_SEEDER__METRICS__ENDPOINT_ADDR=0.0.0.0:9999"
ExecStart=/opt/zebra-seeder/target/release/zebra-seeder start
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

**4. Enable and start:**
```bash
sudo systemctl daemon-reload
sudo systemctl enable zebra-seeder
sudo systemctl start zebra-seeder
```

### DNS Setup

**Example DNS zone configuration:**

```bind
; Delegate seeder.example.com to your server
seeder.example.com.     IN  NS      ns1.seeder.example.com.
ns1.seeder.example.com. IN  A       203.0.113.10

; The seeder will answer for these:
mainnet.seeder.example.com. IN NS ns1.seeder.example.com.
testnet.seeder.example.com. IN NS ns1.seeder.example.com.
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

# Allow metrics (from monitoring network only)
ufw allow from 10.0.0.0/8 to any port 9999 proto tcp

# Allow outbound to Zcash network
# (Usually no action needed for outbound)
```

### Security Checklist

- ✅ Rate limiting enabled (`rate_limit` configured)
- ✅ Metrics endpoint firewalled (if exposed)
- ✅ Running as non-root user
- ✅ DNS domain validation (automatic)
- ✅ Regular security updates
- ✅ Monitor `seeder_mutex_poisoning_total` metric

## Monitoring & Operations

### Metrics

**Metrics endpoint:** `http://localhost:9999/metrics` (if enabled)

**Critical Metrics to Monitor:**

| Metric | Type | Labels | Description | Alert If |
|--------|------|--------|-------------|----------|
| `seeder_peers_servable` | Gauge | `addr_family=v4\|v6` | Servable peers (recently-live, current-version) | < 10 |
| `seeder_peers_ineligible` | Gauge | `reason=not_recently_live\|not_routable\|wrong_port\|banned\|misbehaving\|services_insufficient` | Excluded peers, by reason | - |
| `seeder_peers_known` | Gauge | - | Total peers in the address book | - |
| `seeder_min_protocol_version` | Gauge | - | Enforced protocol-version floor | changes only at a network upgrade |
| `seeder_build_info` | Gauge | `version`, `network` | Build and network identification | - |
| `seeder_mutex_poisoning_total` | Counter | `location=cache_updater\|metrics_logger` | Mutex poisoning events | > 0 |
| `seeder_dns_rate_limited_total` | Counter | - | Rate-limited queries | Spike indicates attack |
| `seeder_dns_errors_total` | Counter | - | DNS errors | > 0 (sustained) |
| `seeder_dns_queries_total` | Counter | `record_type=A\|AAAA` | Total queries | - |
| `seeder_dns_response_peers` | Histogram | - | Peers per response | - |

### Sample Prometheus Queries

**Servable peer count:**
```promql
seeder_peers_servable{addr_family="v4"}
seeder_peers_servable{addr_family="v6"}
```

**Query rate (queries/sec):**
```promql
rate(seeder_dns_queries_total[5m])
```

**Rate limiting rate:**
```promql
rate(seeder_dns_rate_limited_total[5m])
```

**Average peers per response:**
```promql
rate(seeder_dns_response_peers_sum[5m]) / rate(seeder_dns_response_peers_count[5m])
```

### Alerting Rules

**Example Prometheus alerts:**

```yaml
groups:
  - name: zebra-seeder
    rules:
      - alert: SeederLowPeerCount
        expr: seeder_peers_servable < 10
        for: 15m
        annotations:
          summary: "Seeder has low peer count"
          
      - alert: SeederMutexPoisoned
        expr: increase(seeder_mutex_poisoning_total[5m]) > 0
        annotations:
          summary: "CRITICAL: Mutex poisoning detected"
          
      - alert: SeederHighRateLimiting
        expr: rate(seeder_dns_rate_limited_total[5m]) > 10
        for: 5m
        annotations:
          summary: "High rate limiting (possible attack)"
```

### Troubleshooting

**No peers returning:**
```bash
# Check peer count
curl -s http://localhost:9999/metrics | grep peers_servable

# Check logs for errors
journalctl -u zebra-seeder -n 100

# Verify network connectivity
dig @seed.electriccoin.co mainnet.z.cash A
```

**DNS not responding:**
```bash
# Verify server is listening
ss -ulnp | grep :53

# Check logs
journalctl -u zebra-seeder -f

# Test locally
dig @127.0.0.1 -p 1053 testnet.seeder.example.com A
```

**Rate limiting too aggressive:**
```bash
# Increase limits (adjust for your traffic)
export ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND="20"
export ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE="40"

# Restart seeder
systemctl restart zebra-seeder
```

**High memory usage:**
```bash
# Check address book size
curl -s http://localhost:9999/metrics | grep peers_known

# Clear cache if needed (will rebuild)
rm -rf ~/.cache/zebra/network/*
systemctl restart zebra-seeder
```

### Maintenance

**Viewing logs:**
```bash
# Systemd
journalctl -u zebra-seeder -f

# Docker
docker-compose logs -f seeder
```

**Restarting:**
```bash
# Systemd
systemctl restart zebra-seeder

# Docker
docker-compose restart seeder
```

**Upgrading:**
```bash
# Systemd
sudo systemctl stop zebra-seeder
cd /opt/zebra-seeder
git pull
cargo build --release
sudo systemctl start zebra-seeder

# Docker
docker-compose pull
docker-compose up -d
```

**Cache persistence:**
- Address book cached in `~/.cache/zebra/network/`
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
