# Operations Guide

This guide covers production-style deployment, DNS delegation, service
configuration, monitoring, and routine maintenance for Zeeder.

Zeeder runs one crawler and serves one DNS seed domain per process. A mainnet
seeder and a testnet seeder should normally be separate services, with separate
DNS names, separate cache volumes, and separate metrics endpoints. You can run
both services on the same infrastructure, but each service still needs its own
public address on port 53 unless a future version adds multi-zone serving.

## Deployment Model

### One Service Per Network

Run one Zeeder process for each Zcash network you want to serve:

| Network | Example seed domain | Example nameserver | Default P2P port |
|---------|---------------------|--------------------|------------------|
| Mainnet | `mainnet.seeder.example.com` | `ns-mainnet.seeder.example.com` | `8233` |
| Testnet | `testnet.seeder.example.com` | `ns-testnet.seeder.example.com` | `18233` |

This keeps the operational boundary clear. Each process has one
`crawler.network`, one `dns.domain`, and one peer cache. If mainnet has a bad
cache, a high query rate, or a P2P connectivity issue, testnet does not share
that state.

### Public Addressing

A DNS seeder is an authoritative nameserver. Recursive resolvers must be able to
send UDP and TCP DNS queries directly to the seeder on port 53.

Use this model for production:

1. Give each Zeeder service a static public IP address.
2. Publish an A or AAAA record for that service's `dns.nameserver`.
3. Delegate the exact `dns.domain` to that nameserver.
4. Add more independent seeders by adding more NS records.

Do not rely on HTTP load balancers, CDNs, Cloud Run, or reverse proxies for the
authoritative DNS path. DNS already has a load-distribution mechanism: publish
multiple NS records that point at independent seeders. A network-level load
balancer can work only if it passes UDP and TCP 53 correctly, but it adds a
shared failure point that DNS delegation can usually avoid.

### Mainnet And Testnet Separation

Use separate mainnet and testnet IPs by default. Some operators also keep the
two networks in separate subnets or projects so firewall policy, monitoring,
and incident response can be managed independently.

Sharing a VPC, subnet, or CIDR is acceptable when the operator wants a smaller
deployment footprint, but keep these resources separate:

- public IP address per Zeeder service
- service process or container
- peer cache volume
- metrics endpoint
- DNS delegation record

Do not point mainnet and testnet delegations at the same public IP unless a
single process on that IP can serve both domains. Current Zeeder cannot do that;
it serves one configured `dns.domain` and refuses names outside that domain.

### High Availability

For production, run at least 2 independent mainnet seeders in different zones or
regions. Do the same for testnet if testnet availability matters to your users.

Example mainnet delegation with 3 seeders:

```bind
mainnet.seeder.example.com. IN NS ns1-mainnet.seeder.example.com.
mainnet.seeder.example.com. IN NS ns2-mainnet.seeder.example.com.
mainnet.seeder.example.com. IN NS ns3-mainnet.seeder.example.com.

ns1-mainnet.seeder.example.com. IN A 203.0.113.10
ns2-mainnet.seeder.example.com. IN A 203.0.113.20
ns3-mainnet.seeder.example.com. IN A 198.51.100.30
```

Each seeder crawls independently. The DNS client or recursive resolver chooses
which nameserver to query, and it can fail over if one nameserver is unavailable.

## Configuration

### Configuration Sources

Configuration is loaded in priority order:

1. Environment variables (`ZEEDER__*`)
2. TOML config file
3. Hardcoded defaults

The highest-priority source wins. Zeeder rejects unknown config fields, so a
typo such as `dns.tttl` fails startup instead of silently using a default.

### Environment Variables

Prefix all variables with `ZEEDER__` and use double underscores for nesting.
Zeeder does not use Zebra's `ZEBRA_` namespace, so colocated `zebrad` processes
can keep their own `ZEBRA_*` configuration without reading Zeeder settings.

```bash
# Core settings
ZEEDER__DNS__LISTEN_ADDR="0.0.0.0:53"
ZEEDER__DNS__DOMAIN="mainnet.seeder.example.com"
ZEEDER__DNS__NAMESERVER="ns-mainnet.seeder.example.com"
ZEEDER__DNS__TTL="600"

# Crawler
ZEEDER__CRAWLER__NETWORK="Mainnet"  # or "Testnet"

# Rate limiting
ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND="10"
ZEEDER__RATE_LIMIT__BURST_SIZE="20"

# Metrics
ZEEDER__METRICS__ENDPOINT_ADDR="127.0.0.1:9999"
```

The `.env` file is optional. If it exists, it must parse successfully or the
seeder exits before loading configuration.

### TOML Config File

Use a TOML file when you want source-controlled config or systemd `ExecStart`
arguments instead of many environment variables.

```toml
[dns]
listen_addr = "0.0.0.0:53"
domain = "mainnet.seeder.example.com"
nameserver = "ns-mainnet.seeder.example.com"
ttl = 600

[crawler]
network = "Mainnet"

[rate_limit]
queries_per_second = 10
burst_size = 20

[metrics]
endpoint_addr = "127.0.0.1:9999"
```

Run with:

```bash
zeeder start --config /etc/zeeder/mainnet.toml
```

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

### Nameserver Rules

`dns.nameserver` must be outside `dns.domain`. For example, if
`dns.domain = "mainnet.seeder.example.com"`, use a nameserver such as
`ns-mainnet.seeder.example.com`, not `ns.mainnet.seeder.example.com`.

This rule keeps the authority self-consistent. Zeeder answers A and AAAA only
for the exact seed domain, so it does not serve glue or address records for a
nameserver hostname inside that same seed domain.

## Docker Deployment

Docker is the simplest deployment path when the host has Docker or Docker
Compose available. The container runs as a non-root user and listens on port
1053 inside the container, while the host maps public port 53 to that internal
port.

### Single Mainnet Seeder

Create `compose.mainnet.yml`:

```yaml
name: zeeder-mainnet

services:
  mainnet:
    image: zeeder
    restart: unless-stopped
    ports:
      - "53:1053/udp"
      - "53:1053/tcp"
      - "127.0.0.1:9999:9999/tcp"
    volumes:
      - zeeder-mainnet-cache:/cache
    environment:
      ZEEDER__CRAWLER__NETWORK: "Mainnet"
      ZEEDER__DNS__LISTEN_ADDR: "0.0.0.0:1053"
      ZEEDER__DNS__DOMAIN: "mainnet.seeder.example.com"
      ZEEDER__DNS__NAMESERVER: "ns-mainnet.seeder.example.com"
      ZEEDER__DNS__TTL: "600"
      ZEEDER__METRICS__ENDPOINT_ADDR: "0.0.0.0:9999"

volumes:
  zeeder-mainnet-cache:
```

Build and start:

```bash
docker build -t zeeder .
docker compose -f compose.mainnet.yml up -d
```

Verify the container:

```bash
docker compose -f compose.mainnet.yml logs --tail=100 mainnet
dig @127.0.0.1 mainnet.seeder.example.com SOA
curl -s http://127.0.0.1:9999/metrics | grep 'zeeder_build_info'
```

### Mainnet And Testnet On One Host

If one host has 2 public IP addresses, bind each service to a different host IP
on port 53. Replace the example IPs with addresses assigned to the host.

```yaml
name: zeeder-dual-network

services:
  mainnet:
    image: zeeder
    restart: unless-stopped
    ports:
      - "203.0.113.10:53:1053/udp"
      - "203.0.113.10:53:1053/tcp"
      - "127.0.0.1:9999:9999/tcp"
    volumes:
      - zeeder-mainnet-cache:/cache
    environment:
      ZEEDER__CRAWLER__NETWORK: "Mainnet"
      ZEEDER__DNS__LISTEN_ADDR: "0.0.0.0:1053"
      ZEEDER__DNS__DOMAIN: "mainnet.seeder.example.com"
      ZEEDER__DNS__NAMESERVER: "ns-mainnet.seeder.example.com"
      ZEEDER__DNS__TTL: "600"
      ZEEDER__METRICS__ENDPOINT_ADDR: "0.0.0.0:9999"

  testnet:
    image: zeeder
    restart: unless-stopped
    ports:
      - "203.0.113.20:53:1053/udp"
      - "203.0.113.20:53:1053/tcp"
      - "127.0.0.1:10099:9999/tcp"
    volumes:
      - zeeder-testnet-cache:/cache
    environment:
      ZEEDER__CRAWLER__NETWORK: "Testnet"
      ZEEDER__DNS__LISTEN_ADDR: "0.0.0.0:1053"
      ZEEDER__DNS__DOMAIN: "testnet.seeder.example.com"
      ZEEDER__DNS__NAMESERVER: "ns-testnet.seeder.example.com"
      ZEEDER__DNS__TTL: "300"
      ZEEDER__METRICS__ENDPOINT_ADDR: "0.0.0.0:9999"

volumes:
  zeeder-mainnet-cache:
  zeeder-testnet-cache:
```

This shares the host and possibly the subnet, but it does not share the public
IP, cache, metrics port, or process state.

## Bare Metal Deployment

Use bare metal when you want Zeeder managed directly by systemd. The service
needs permission to bind port 53 and a writable cache directory for
zebra-network's peer cache.

### Install

Build Zeeder as an operator with a Rust toolchain, then install only the binary
and configuration on the host:

```bash
sudo useradd --system --home /var/lib/zeeder --shell /usr/sbin/nologin zeeder
sudo install -d -o zeeder -g zeeder /var/lib/zeeder
sudo install -d -o root -g root /opt/zeeder /etc/zeeder

git clone https://github.com/ZcashFoundation/dnsseederNG zeeder
cd zeeder
cargo build --release
sudo install -m 0755 target/release/zeeder /opt/zeeder/zeeder
```

Create `/etc/zeeder/mainnet.toml`:

```toml
[dns]
listen_addr = "0.0.0.0:53"
domain = "mainnet.seeder.example.com"
nameserver = "ns-mainnet.seeder.example.com"
ttl = 600

[crawler]
network = "Mainnet"

[rate_limit]
queries_per_second = 10
burst_size = 20

[metrics]
endpoint_addr = "127.0.0.1:9999"
```

Create `/etc/systemd/system/zeeder-mainnet.service`:

```ini
[Unit]
Description=Zeeder DNS seeder for Zcash mainnet
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=zeeder
Group=zeeder
Environment="XDG_CACHE_HOME=/var/cache/zeeder-mainnet"
ExecStart=/opt/zeeder/zeeder start --config /etc/zeeder/mainnet.toml
Restart=on-failure
RestartSec=10
KillSignal=SIGINT
TimeoutStopSec=60
CacheDirectory=zeeder-mainnet
NoNewPrivileges=true
ProtectHome=true
ProtectSystem=strict
ReadWritePaths=/var/cache/zeeder-mainnet
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

Start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now zeeder-mainnet
sudo systemctl status zeeder-mainnet
```

For testnet, create a second TOML file and service unit with a separate
`dns.domain`, `dns.nameserver`, `crawler.network`, cache directory, and metrics
port. If both services run on one host, they also need separate public IPs
because both must bind port 53.

## DNS Setup

Configure DNS in the parent zone that owns your seed domains. The parent zone
must publish both the delegation and the nameserver address record.

### Single Mainnet Seeder

Example parent-zone records:

```bind
mainnet.seeder.example.com. IN NS ns-mainnet.seeder.example.com.
ns-mainnet.seeder.example.com. IN A 203.0.113.10
```

The Zeeder service at `203.0.113.10` should use:

```toml
[dns]
domain = "mainnet.seeder.example.com"
nameserver = "ns-mainnet.seeder.example.com"
```

### Separate Mainnet And Testnet Seeders

Use separate public IPs when both networks are served by Zeeder:

```bind
mainnet.seeder.example.com. IN NS ns-mainnet.seeder.example.com.
ns-mainnet.seeder.example.com. IN A 203.0.113.10

testnet.seeder.example.com. IN NS ns-testnet.seeder.example.com.
ns-testnet.seeder.example.com. IN A 203.0.113.20
```

The mainnet Zeeder process should answer only `mainnet.seeder.example.com`, and
the testnet Zeeder process should answer only `testnet.seeder.example.com`.

### High Availability DNS

Add more NS records for each network when you deploy more seeders:

```bind
mainnet.seeder.example.com. IN NS ns1-mainnet.seeder.example.com.
mainnet.seeder.example.com. IN NS ns2-mainnet.seeder.example.com.

ns1-mainnet.seeder.example.com. IN A 203.0.113.10
ns2-mainnet.seeder.example.com. IN A 198.51.100.10
```

Every nameserver listed for `mainnet.seeder.example.com` must run a Zeeder
process configured for `mainnet.seeder.example.com`.

### Delegation Verification

After updating DNS, verify both the parent delegation and the seeder response:

```bash
dig mainnet.seeder.example.com NS +short
dig ns-mainnet.seeder.example.com A +short
dig @203.0.113.10 mainnet.seeder.example.com SOA
dig @203.0.113.10 mainnet.seeder.example.com A +short
```

For each additional nameserver IP, run the direct `dig @<ip>` checks. Do not
only test recursive resolution, because recursive resolvers may cache old
answers or hide one failed nameserver behind retries.

### DNS Provider Notes

Most DNS providers expose this as a delegated subdomain or NS record. Add the
NS record for the seed domain, then add A or AAAA records for each nameserver
hostname in the parent zone.

Prefer nameserver hostnames that are outside the delegated seed domain, such as
`ns-mainnet.seeder.example.com` for `mainnet.seeder.example.com`. Do not choose
`ns1.mainnet.seeder.example.com` unless another authoritative server can answer
its address record, because Zeeder does not serve nameserver glue inside its
own `dns.domain`.

## Firewall Rules

Open DNS to the internet and keep metrics private:

```bash
# Allow DNS queries
ufw allow 53/udp
ufw allow 53/tcp

# Allow metrics from the monitoring network only
ufw allow from 10.0.0.0/8 to any port 9999 proto tcp

# Optional: allow inbound P2P crawler connections for the network served here
ufw allow 8233/tcp   # mainnet
ufw allow 18233/tcp  # testnet
```

Outbound connections to the Zcash P2P network must also be allowed. Most hosts
allow outbound traffic by default, but locked-down environments need egress to
port 8233 for mainnet or 18233 for testnet.

## Monitoring

### Health Checks

Liveness checks should ask the DNS server for zone metadata:

```bash
dig @127.0.0.1 -p 1053 testnet.seeder.example.com SOA
```

Readiness checks should use the metrics endpoint. The crawler is ready to serve
bootstrap peers when at least one address family reports a
`non-zero servable-peer gauge`:

```bash
curl -s http://localhost:9999/metrics | grep 'zeeder_peers_servable'
```

A zero gauge means DNS can still answer SOA, NS, and NODATA responses, but A or
AAAA bootstrap answers will be empty for that address family.

### Metrics

Metrics are exposed at `/metrics` when `metrics.endpoint_addr` is configured.

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

### Prometheus Queries

```promql
# Servable peer count
zeeder_peers_servable{addr_family="v4"}
zeeder_peers_servable{addr_family="v6"}

# Query rate
rate(zeeder_dns_queries_total[5m])

# Rate limiting rate
rate(zeeder_dns_rate_limited_total[5m])

# Average peers per response
rate(zeeder_dns_response_peers_sum[5m]) / rate(zeeder_dns_response_peers_count[5m])
```

### Alerting Rules

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
          summary: "Mutex poisoning detected"

      - alert: SeederHighRateLimiting
        expr: rate(zeeder_dns_rate_limited_total[5m]) > 10
        for: 5m
        annotations:
          summary: "High rate limiting"
```

## Troubleshooting

### No A Or AAAA Answers

Check the servable peer gauges first:

```bash
curl -s http://localhost:9999/metrics | grep 'zeeder_peers_servable'
curl -s http://localhost:9999/metrics | grep 'zeeder_peers_known'
```

If known peers are present but servable peers are low, inspect the unservable
reason gauges:

```bash
curl -s http://localhost:9999/metrics | grep 'zeeder_peers_unservable'
```

Common causes are peers on the wrong port, peers that have not handshaked
recently, peers that do not advertise `NODE_NETWORK`, and environments with no
outbound P2P access.

### DNS Does Not Respond

Check whether Zeeder is listening on UDP and TCP 53:

```bash
ss -ulnp | grep :53
ss -tlnp | grep :53
```

Then check logs:

```bash
journalctl -u zeeder-mainnet -n 100
docker compose logs --tail=100 mainnet
```

Finally, query the service directly:

```bash
dig @127.0.0.1 -p 1053 testnet.seeder.example.com A
```

### Delegation Is Wrong

Query the parent records and each authoritative IP directly:

```bash
dig mainnet.seeder.example.com NS +short
dig ns-mainnet.seeder.example.com A +short
dig @203.0.113.10 mainnet.seeder.example.com SOA
```

If direct queries work but recursive queries fail, wait for parent-zone TTLs to
expire and check that every NS target has an address record.

### Rate Limiting Is Too Aggressive

Increase the per-IP rate and burst size, then restart:

```bash
export ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND="20"
export ZEEDER__RATE_LIMIT__BURST_SIZE="40"
systemctl restart zeeder-mainnet
```

Watch `zeeder_dns_rate_limited_total` after the change.

### Cache Needs A Reset

The peer cache is only a startup accelerator. It is safe to delete, and Zeeder
will rebuild it from the network.

For Docker:

```bash
docker compose -f compose.mainnet.yml down
docker volume rm zeeder-mainnet_zeeder-mainnet-cache
docker compose -f compose.mainnet.yml up -d
```

For systemd:

```bash
sudo systemctl stop zeeder-mainnet
sudo rm -rf /var/cache/zeeder-mainnet/zebra/network/*
sudo systemctl start zeeder-mainnet
```

## Maintenance

### Logs

```bash
journalctl -u zeeder-mainnet -f
docker compose -f compose.mainnet.yml logs -f mainnet
```

### Restart

```bash
sudo systemctl restart zeeder-mainnet
docker compose -f compose.mainnet.yml restart mainnet
```

### Upgrade

For systemd:

```bash
sudo systemctl stop zeeder-mainnet
cd zeeder
git pull
cargo build --release
sudo install -m 0755 target/release/zeeder /opt/zeeder/zeeder
sudo systemctl start zeeder-mainnet
```

For Docker:

```bash
docker build -t zeeder .
docker compose -f compose.mainnet.yml up -d
```

### Cache Persistence

- The systemd unit above sets `XDG_CACHE_HOME=/var/cache/zeeder-mainnet`, so
  the peer cache lives under `/var/cache/zeeder-mainnet/zebra/network/`.
- Bare-metal processes without `XDG_CACHE_HOME` use `~/.cache/zebra/network/`.
- Docker sets `XDG_CACHE_HOME=/cache`, so the peer cache lives under
  `/cache/zebra/network/`.
- Persisting the cache speeds up restart, but deleting it is safe.

## Capacity Planning

Expected usage is small for normal DNS seeder traffic:

- RAM: about 50 to 100 MB
- CPU: under 5% of one core
- Disk: under 10 MB for the peer cache
- Network: about 1 to 10 Mbps, depending on DNS query volume and P2P activity

Scale by adding independent seeders and publishing additional NS records. Keep
metrics and logs per seeder so one unhealthy instance does not disappear behind
aggregate fleet numbers.
