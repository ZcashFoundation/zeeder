# Operations Guide

This guide covers production-style deployment, DNS delegation, service
configuration, monitoring, and routine maintenance for Zeeder.

One Zeeder process serves every network you configure. It runs one crawler per
network and answers all of their DNS zones on a single listener. A typical
deployment serves both `mainnet.seeder.example.com` and
`testnet.seeder.example.com` from one process on one public IP, the same shape
the earlier CoreDNS-based seeder offered.

## Deployment Model

### One Process, Every Network

Declare a zone per network under `[zones.<network>]`. Each zone binds a Zcash
network to its authoritative DNS identity:

| Network | Example seed domain | Example nameserver | P2P port |
|---------|---------------------|--------------------|----------|
| Mainnet | `mainnet.seeder.example.com` | `ns-mainnet.seeder.example.com` | `8233` |
| Testnet | `testnet.seeder.example.com` | `ns-testnet.seeder.example.com` | `18233` |

The process runs an independent crawler for each network. Each crawler keeps its
own peer cache, its own address book, and its own protocol-version floor, so a
mainnet connectivity problem does not change which testnet peers are served. The
two crawlers bind different P2P ports (`8233` and `18233`), so they never
conflict.

Mainnet and testnet are the two networks that can share a process. Regtest uses
the same P2P port as testnet, so it cannot run alongside testnet in one process.

### Public Addressing

A DNS seeder is an authoritative nameserver. Recursive resolvers must be able to
send UDP and TCP DNS queries directly to the seeder on port 53.

Use this model for production:

1. Give the Zeeder service a static public IP address.
2. Publish an A or AAAA record for each zone's `nameserver`.
3. Delegate each zone's `domain` to the matching nameserver.
4. Add more independent seeders by adding more NS records.

Both zones can delegate to the same public IP because one process answers both.
Do not rely on HTTP load balancers, CDNs, Cloud Run, or reverse proxies for the
authoritative DNS path. DNS already has a load-distribution mechanism: publish
multiple NS records that point at independent seeders.

### High Availability

For production, run at least 2 independent seeder processes in different zones or
regions. Each process serves every configured network, so high availability is a
matter of running more processes, not more services per network.

Example mainnet delegation with 3 seeders:

```bind
mainnet.seeder.example.com. IN NS ns1-mainnet.seeder.example.com.
mainnet.seeder.example.com. IN NS ns2-mainnet.seeder.example.com.
mainnet.seeder.example.com. IN NS ns3-mainnet.seeder.example.com.

ns1-mainnet.seeder.example.com. IN A 203.0.113.10
ns2-mainnet.seeder.example.com. IN A 203.0.113.20
ns3-mainnet.seeder.example.com. IN A 198.51.100.30
```

Each seeder crawls independently. The recursive resolver chooses which
nameserver to query and fails over if one is unavailable.

## Configuration

### Configuration Sources

Configuration is loaded in priority order:

1. Environment variables (`ZEEDER__*`)
2. TOML config file
3. Hardcoded defaults

The highest-priority source wins. Zeeder rejects unknown config fields, so a
typo such as `zones.mainnet.tttl` fails startup instead of silently using a
default. Zeeder also requires at least one zone, so a config with no
`[zones.<network>]` entry fails fast.

### Environment Variables

Prefix all variables with `ZEEDER__` and use double underscores for nesting. The
network name is part of the key, so each zone gets its own subtree. Zeeder does
not use Zebra's `ZEBRA_` namespace, so colocated `zebrad` processes keep their
own `ZEBRA_*` configuration.

```bash
# Shared DNS listener for every zone
ZEEDER__DNS__LISTEN_ADDR="0.0.0.0:53"

# Mainnet zone
ZEEDER__ZONES__MAINNET__DOMAIN="mainnet.seeder.example.com"
ZEEDER__ZONES__MAINNET__NAMESERVER="ns-mainnet.seeder.example.com"
ZEEDER__ZONES__MAINNET__TTL="600"

# Testnet zone
ZEEDER__ZONES__TESTNET__DOMAIN="testnet.seeder.example.com"
ZEEDER__ZONES__TESTNET__NAMESERVER="ns-testnet.seeder.example.com"
ZEEDER__ZONES__TESTNET__TTL="300"

# Rate limiting
ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND="10"
ZEEDER__RATE_LIMIT__BURST_SIZE="20"

# Metrics and health
ZEEDER__METRICS__ENDPOINT_ADDR="127.0.0.1:9999"
ZEEDER__HEALTH__ENDPOINT_ADDR="0.0.0.0:8080"
```

TOML zone keys are lowercase (`mainnet`, `testnet`). Environment variable keys
are conventionally uppercase, and Zeeder lowercases those key segments while
loading them, so `ZEEDER__ZONES__MAINNET__DOMAIN` maps to `zones.mainnet.domain`.

The `.env` file is optional. If it exists, it must parse successfully or the
seeder exits before loading configuration.

### TOML Config File

Use a TOML file when you want source-controlled config or systemd `ExecStart`
arguments instead of many environment variables.

```toml
[dns]
listen_addr = "0.0.0.0:53"

[zones.mainnet]
domain = "mainnet.seeder.example.com"
nameserver = "ns-mainnet.seeder.example.com"
ttl = 600

[zones.testnet]
domain = "testnet.seeder.example.com"
nameserver = "ns-testnet.seeder.example.com"
ttl = 300

[rate_limit]
queries_per_second = 10
burst_size = 20

[metrics]
endpoint_addr = "127.0.0.1:9999"

[health]
endpoint_addr = "0.0.0.0:8080"
ready_threshold = 1
```

Run with:

```bash
zeeder start --config /etc/zeeder/zeeder.toml
```

### Configuration Reference

`<network>` is the lowercase network name (`mainnet` or `testnet`). Repeat the
`zones.<network>.*` keys once per network you serve.

| Parameter | Environment Variable | Default | Description |
|-----------|---------------------|---------|-------------|
| `dns.listen_addr` | `ZEEDER__DNS__LISTEN_ADDR` | `0.0.0.0:53` | Shared DNS listener for every zone |
| `zones.<network>.domain` | `ZEEDER__ZONES__<NETWORK>__DOMAIN` | (none) | Authoritative domain for that network |
| `zones.<network>.nameserver` | `ZEEDER__ZONES__<NETWORK>__NAMESERVER` | (none) | Out-of-zone authoritative nameserver |
| `zones.<network>.ttl` | `ZEEDER__ZONES__<NETWORK>__TTL` | `600` | DNS response TTL in seconds |
| `rate_limit.queries_per_second` | `ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND` | `10` | Max queries/sec per IP; must be greater than 0 |
| `rate_limit.burst_size` | `ZEEDER__RATE_LIMIT__BURST_SIZE` | `20` | Burst capacity; must be greater than 0 |
| `metrics.endpoint_addr` | `ZEEDER__METRICS__ENDPOINT_ADDR` | (disabled) | Prometheus endpoint |
| `health.endpoint_addr` | `ZEEDER__HEALTH__ENDPOINT_ADDR` | (disabled) | Health and readiness endpoint |
| `health.ready_threshold` | `ZEEDER__HEALTH__READY_THRESHOLD` | `1` | Servable peers per zone required for readiness |

### Zone Rules

Each zone's `nameserver` must be outside its own `domain`. For example, if
`zones.mainnet.domain = "mainnet.seeder.example.com"`, use a nameserver such as
`ns-mainnet.seeder.example.com`, not `ns.mainnet.seeder.example.com`.

This rule keeps the authority self-consistent. Zeeder answers A and AAAA only for
the exact seed domain, so it does not serve glue or address records for a
nameserver hostname inside that same zone.

Zone domains must not overlap. Zeeder rejects a configuration where one zone's
domain is equal to or nested inside another, so every query routes to exactly one
zone.

## Docker Deployment

Docker is the simplest deployment path. The container runs as a non-root user and
listens on port 1053 inside the container, while the host maps public port 53 to
that internal port. One container serves every configured zone.

Create `compose.yml`:

```yaml
name: zeeder

services:
  seeder:
    image: zfnd/dnsseeder
    restart: unless-stopped
    ports:
      - "53:1053/udp"
      - "53:1053/tcp"
      - "127.0.0.1:9999:9999/tcp"
      - "127.0.0.1:8080:8080/tcp"
    volumes:
      - zeeder-cache:/cache
    environment:
      ZEEDER__DNS__LISTEN_ADDR: "0.0.0.0:1053"
      ZEEDER__ZONES__MAINNET__DOMAIN: "mainnet.seeder.example.com"
      ZEEDER__ZONES__MAINNET__NAMESERVER: "ns-mainnet.seeder.example.com"
      ZEEDER__ZONES__MAINNET__TTL: "600"
      ZEEDER__ZONES__TESTNET__DOMAIN: "testnet.seeder.example.com"
      ZEEDER__ZONES__TESTNET__NAMESERVER: "ns-testnet.seeder.example.com"
      ZEEDER__ZONES__TESTNET__TTL: "300"
      ZEEDER__METRICS__ENDPOINT_ADDR: "0.0.0.0:9999"
      ZEEDER__HEALTH__ENDPOINT_ADDR: "0.0.0.0:8080"

volumes:
  zeeder-cache:
```

Pull and start:

```bash
docker compose -f compose.yml pull
docker compose -f compose.yml up -d
```

To run an unreleased build, build the image locally and point `image:` at it:

```bash
docker build -t zeeder .
```

Verify the container:

```bash
docker compose -f compose.yml logs --tail=100 seeder
dig @127.0.0.1 mainnet.seeder.example.com SOA
dig @127.0.0.1 testnet.seeder.example.com SOA
curl -s http://127.0.0.1:9999/metrics | grep 'zeeder_build_info'
curl -s http://127.0.0.1:8080/ready
```

One container crawls both networks, so it holds roughly twice the peer
connections of a single-network process. The peer caches stay separate inside the
shared volume because Zebra keeps them in per-network files.

### Image Verification

Release images are Cosign-signed and carry build-provenance and SBOM
attestations. Verify a pulled image before trusting it:

```bash
cosign verify docker.io/zfnd/dnsseeder:latest \
  --certificate-identity-regexp='^https://github\.com/ZcashFoundation/zeeder/\.github/workflows/release\.yml@' \
  --certificate-oidc-issuer='https://token.actions.githubusercontent.com'

gh attestation verify oci://docker.io/zfnd/dnsseeder:latest \
  --repo ZcashFoundation/zeeder
```

## Bare Metal Deployment

Use bare metal when you want Zeeder managed directly by systemd. The service
needs permission to bind port 53 and a writable cache directory for
zebra-network's per-network peer caches.

### Install

Build Zeeder as an operator with a Rust toolchain, then install only the binary
and configuration on the host:

```bash
sudo useradd --system --home /var/lib/zeeder --shell /usr/sbin/nologin zeeder
sudo install -d -o zeeder -g zeeder /var/lib/zeeder
sudo install -d -o root -g root /opt/zeeder /etc/zeeder

git clone https://github.com/ZcashFoundation/zeeder
cd zeeder
cargo build --release
sudo install -m 0755 target/release/zeeder /opt/zeeder/zeeder
```

Create `/etc/zeeder/zeeder.toml`:

```toml
[dns]
listen_addr = "0.0.0.0:53"

[zones.mainnet]
domain = "mainnet.seeder.example.com"
nameserver = "ns-mainnet.seeder.example.com"
ttl = 600

[zones.testnet]
domain = "testnet.seeder.example.com"
nameserver = "ns-testnet.seeder.example.com"
ttl = 300

[rate_limit]
queries_per_second = 10
burst_size = 20

[metrics]
endpoint_addr = "127.0.0.1:9999"

[health]
endpoint_addr = "0.0.0.0:8080"
ready_threshold = 1
```

Create `/etc/systemd/system/zeeder.service`:

```ini
[Unit]
Description=Zeeder DNS seeder for Zcash
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=zeeder
Group=zeeder
Environment="XDG_CACHE_HOME=/var/cache/zeeder"
ExecStart=/opt/zeeder/zeeder start --config /etc/zeeder/zeeder.toml
Restart=on-failure
RestartSec=10
KillSignal=SIGINT
TimeoutStopSec=60
CacheDirectory=zeeder
NoNewPrivileges=true
ProtectHome=true
ProtectSystem=strict
ReadWritePaths=/var/cache/zeeder
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

Start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now zeeder
sudo systemctl status zeeder
```

One unit serves every configured network. The crawlers write per-network peer
caches under `XDG_CACHE_HOME/zebra/network/` (for example `mainnet.peers` and
`testnet.peers`), so they share the cache directory without colliding.

## GCP Fleet

The Zcash Foundation runs Zeeder as six authoritative nameservers on
Container-Optimized OS VMs. This section documents that production fleet: its
topology, its privilege model, and the roll that ships a new image without
dropping a delegation.

### Topology

The fleet is six authoritative nameservers on Container-Optimized OS VMs, one per
region across the United States and Europe. Each VM holds a reserved static
external IP. The parent zone publishes those IPs as `ns1.zfnd.org` through
`ns6.zfnd.org` and NS-delegates `mainnet.seeder.zfnd.org` and
`testnet.seeder.zfnd.org` to them. The delegation records are managed outside
this repository.

The fleet inventory (project, VM names, zones) lives in `deploy/gcp/fleet.conf`,
which is not committed; `deploy/gcp/fleet.conf.example` documents its shape.

### Privilege Model

Zeeder runs non-root as uid `65532` and listens on `:1053`. The host startup
script stops `systemd-resolved` and redirects `:53` to `:1053` for both UDP and
TCP with iptables. The script re-runs on every boot, so the redirect is
re-established after each reboot without manual repair. It also mounts
`/var/lib/zeeder/cache` (owned by uid `65532`) at the container's `/cache`, so
zebra-network's per-network peer caches survive container recreation and a
reset/reboot/roll resumes the crawler from its last known peers instead of cold.

### Roll Constraints

A roll updates one VM at a time. Before it moves to the next VM, it digs both
zones over UDP and TCP against the VM it just updated. Mainnet is hard-gated: if
the node is not serving mainnet over both UDP and TCP the roll aborts. Testnet is
soft-gated: because the NU6.3 protocol floor makes testnet servability
network-dependent, a freshly reset crawler can sit at `servable=0` for a while
while mainnet is healthy, so a cold testnet only warns and the roll continues.
`ns1` is the oldest VM and rolls last. Because the roll stops at the first mainnet
failure, a bad image reaches at most one nameserver while the other five keep
answering.

### Deploying a New Image

1. `release.yml` publishes a Cosign-signed image and its `sha256` digest.
2. A pull request bumps `deploy/gcp/IMAGE` to that digest. The merge is the
   deploy event, and the file's git history is the audit trail.
3. An operator runs `deploy/gcp/seeders.sh --roll`. Add `--only <ns>` to roll the
   canary alone, and `--dry-run` to preview the plan without touching a VM.
4. The roll runs `cosign verify` on the pinned digest before it touches any VM,
   so an unsigned or mismatched digest never rolls.
5. `deploy/gcp/seeders.sh --status` is the post-roll check.

Rollback is a `git revert` of `deploy/gcp/IMAGE` followed by another
`--roll`.

### CI-driven deploy

The `Deploy fleet` workflow (`.github/workflows/deploy.yml`) runs the same
`seeders.sh --roll` from CI, so an operator does not need `gcloud`, `cosign`, or
the fleet inventory on their laptop. It is `workflow_dispatch` only, with a
`dry_run` toggle (default on) and an optional `only` input; the local
`seeders.sh --roll` remains the fallback and is unchanged.

The run is human-gated twice: the manual dispatch, and the `production`
environment's required reviewer. Cloud access is keyless, and the fleet
inventory is injected from a repository variable into a runner-temp file
(`FLEET_CONF_FILE`) and masked before any step can log it, so no plaintext
inventory ever lands in the repository or a checkout. It is configuration, not
a credential — nothing in it grants access, which is decided by IAM — so a
readable variable keeps it maintainable.

The workflow needs a one-time configuration before it can run: keyless CI
authentication to the fleet project, the variables and secret referenced by
`deploy.yml`, and a `production` environment with required reviewers. This is
repository and cloud admin setup; the specific values are held by the operators.
Verify with a `dry_run: true` dispatch before a real run.

### Image Pull Path

The VMs pull the pinned digest from Docker Hub. During a Docker Hub outage,
`mirror.gcr.io` is the break-glass pull path. A dedicated Artifact Registry
remote repository is the planned primary path.

Verify the fleet image with the commands in
[Image Verification](#image-verification), substituting `$(cat deploy/gcp/IMAGE)`
for the tagged reference.

### Fleet container environment

```env
ZEEDER__DNS__LISTEN_ADDR=0.0.0.0:1053
ZEEDER__ZONES__MAINNET__DOMAIN=mainnet.seeder.zfnd.org
ZEEDER__ZONES__MAINNET__NAMESERVER=<ns>.zfnd.org
ZEEDER__ZONES__MAINNET__TTL=600
ZEEDER__ZONES__TESTNET__DOMAIN=testnet.seeder.zfnd.org
ZEEDER__ZONES__TESTNET__NAMESERVER=<ns>.zfnd.org
ZEEDER__ZONES__TESTNET__TTL=300
ZEEDER__METRICS__ENDPOINT_ADDR=127.0.0.1:9999
ZEEDER__HEALTH__ENDPOINT_ADDR=127.0.0.1:8080
ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND=50
ZEEDER__RATE_LIMIT__BURST_SIZE=100
```

Each VM substitutes its own nameserver (`ns1` through `ns6`) for `<ns>`; the
nameserver is out-of-zone by design.

## DNS Setup

Configure DNS in the parent zone that owns your seed domains. The parent zone
must publish both the delegation and the nameserver address record for each zone.

### Both Networks On One IP

Because one process answers both zones, both delegations can point at the same
public IP:

```bind
mainnet.seeder.example.com. IN NS ns-mainnet.seeder.example.com.
ns-mainnet.seeder.example.com. IN A 203.0.113.10

testnet.seeder.example.com. IN NS ns-testnet.seeder.example.com.
ns-testnet.seeder.example.com. IN A 203.0.113.10
```

The Zeeder process at `203.0.113.10` answers `mainnet.seeder.example.com` from
its mainnet crawler and `testnet.seeder.example.com` from its testnet crawler. A
query for any name outside every configured zone returns REFUSED.

### High Availability DNS

Add more NS records for each network when you deploy more seeder processes:

```bind
mainnet.seeder.example.com. IN NS ns1-mainnet.seeder.example.com.
mainnet.seeder.example.com. IN NS ns2-mainnet.seeder.example.com.

ns1-mainnet.seeder.example.com. IN A 203.0.113.10
ns2-mainnet.seeder.example.com. IN A 198.51.100.10
```

Every nameserver listed for `mainnet.seeder.example.com` must run a Zeeder
process configured with a mainnet zone for that domain.

### Delegation Verification

After updating DNS, verify both the parent delegation and the seeder response for
each zone:

```bash
dig mainnet.seeder.example.com NS +short
dig ns-mainnet.seeder.example.com A +short
dig @203.0.113.10 mainnet.seeder.example.com SOA
dig @203.0.113.10 mainnet.seeder.example.com A +short
dig @203.0.113.10 testnet.seeder.example.com A +short
```

For each additional nameserver IP, run the direct `dig @<ip>` checks. Do not only
test recursive resolution, because recursive resolvers may cache old answers or
hide one failed nameserver behind retries.

### Zebra Client Configuration

Zebra resolves the seed domain, not the nameserver hostname. A Zebra config entry
uses the seed domain plus the Zcash P2P port:

```toml
[network]
initial_mainnet_peers = ["mainnet.seeder.example.com:8233"]
initial_testnet_peers = ["testnet.seeder.example.com:18233"]
```

At startup, Zebra asks the operating system resolver for A and AAAA records for
those hostnames, attaches the configured P2P port to each returned IP, filters
invalid peer addresses, limits initial connection attempts, and then handshakes
with those peers. It does not connect to a zone's `nameserver`; that name exists
so recursive resolvers can find the authoritative DNS server for the seed domain.

### DNS Provider Notes

Most DNS providers expose this as a delegated subdomain or NS record. Add the NS
record for each seed domain, then add A or AAAA records for each nameserver
hostname in the parent zone.

Prefer nameserver hostnames outside the delegated seed domain, such as
`ns-mainnet.seeder.example.com` for `mainnet.seeder.example.com`. Do not choose
`ns1.mainnet.seeder.example.com` unless another authoritative server can answer
its address record, because Zeeder does not serve nameserver glue inside its own
zone.

## Firewall Rules

Open DNS to the internet, keep metrics and health private, and allow outbound
P2P for both networks:

```bash
# Allow DNS queries
ufw allow 53/udp
ufw allow 53/tcp

# Allow metrics and health from the monitoring network only
ufw allow from 10.0.0.0/8 to any port 9999 proto tcp
ufw allow from 10.0.0.0/8 to any port 8080 proto tcp

# Optional: allow inbound P2P crawler connections for each network served here
ufw allow 8233/tcp   # mainnet
ufw allow 18233/tcp  # testnet
```

Outbound connections to the Zcash P2P network must also be allowed. One process
crawls every configured network, so a locked-down host needs egress to both port
8233 (mainnet) and port 18233 (testnet).

## Monitoring

### Health And Readiness

When `health.endpoint_addr` is configured, Zeeder serves two HTTP endpoints:

- `GET /health` returns `200` while the process is running. Use it for liveness.
- `GET /ready` returns `200` only when every zone has at least
  `health.ready_threshold` servable peers, otherwise `503`. The body lists each
  zone's servable-peer count. Use it for readiness.

```bash
curl -s http://localhost:8080/health
curl -s http://localhost:8080/ready
```

A `503` from `/ready` means at least one zone is still warming up. DNS can still
answer SOA, NS, and NODATA for that zone, but A and AAAA answers will be empty
until the crawler finds enough servable peers.

### Metrics

Metrics are exposed at `/metrics` when `metrics.endpoint_addr` is configured.
Per-network metrics carry a `network` label so mainnet and testnet are
distinguishable.

| Metric | Type | Labels | Description | Alert If |
|--------|------|--------|-------------|----------|
| `zeeder_peers_servable` | Gauge | `network=mainnet\|testnet`, `addr_family=v4\|v6` | Servable peers (recently-live, current-version, outbound, clean) | < 10 |
| `zeeder_peers_unservable` | Gauge | `network=mainnet\|testnet`, `reason=not_routable\|wrong_port\|not_recently_live\|not_full_node\|inbound\|misbehaving` | Unservable peers, by reason | - |
| `zeeder_peers_known` | Gauge | `network=mainnet\|testnet` | Total peers in the address book | - |
| `zeeder_min_protocol_version` | Gauge | `network=mainnet\|testnet` | Enforced protocol-version floor | changes only at a network upgrade |
| `zeeder_build_info` | Gauge | `version`, `git_sha`, `network` | Build and network identification | - |
| `zeeder_mutex_poisoning_total` | Counter | `network=mainnet\|testnet` | Mutex poisoning events | > 0 |
| `zeeder_dns_rate_limited_total` | Counter | - | Rate-limited queries | Spike indicates attack |
| `zeeder_dns_errors_total` | Counter | - | DNS errors | > 0 (sustained) |
| `zeeder_dns_queries_total` | Counter | `record_type=A\|AAAA\|SOA\|NS\|other` | Total queries | - |
| `zeeder_dns_response_peers` | Summary | `network=mainnet\|testnet` | Peers per response | - |

The DNS query, error, and rate-limit counters are process-wide: one listener
serves every zone, and rate limiting and error handling happen before a query is
routed to a zone, so these counters carry no `network` label.

zebra-network also emits its own internal metrics (`zcash.net.*`,
`candidate_set.*`, `pool.*`). Those carry no `network` label, so when one process
runs two crawlers their values are combined across networks. Use the
`zeeder_*` metrics above for per-network monitoring.

### Prometheus Queries

```promql
# Servable peer count per network
zeeder_peers_servable{network="mainnet", addr_family="v4"}
zeeder_peers_servable{network="testnet", addr_family="v4"}

# Query rate
rate(zeeder_dns_queries_total[5m])

# Rate limiting rate
rate(zeeder_dns_rate_limited_total[5m])

# Average peers per response, per network
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
          summary: "Seeder has low peer count on {{ $labels.network }}"

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

Check the servable peer gauges first, filtering by the affected network:

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
outbound P2P access for that network's port.

### DNS Does Not Respond

Check whether Zeeder is listening on UDP and TCP 53:

```bash
ss -ulnp | grep :53
ss -tlnp | grep :53
```

Then check logs:

```bash
journalctl -u zeeder -n 100
docker compose logs --tail=100 seeder
```

Finally, query the service directly for each zone:

```bash
dig @127.0.0.1 -p 1053 mainnet.seeder.example.com A
dig @127.0.0.1 -p 1053 testnet.seeder.example.com SOA
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
systemctl restart zeeder
```

Watch `zeeder_dns_rate_limited_total` after the change.

### Cache Needs A Reset

The peer cache is only a startup accelerator. It is safe to delete, and Zeeder
will rebuild it from the network. The caches are per-network files, so you can
clear one network without touching the other.

For Docker:

```bash
docker compose -f compose.yml down
docker volume rm zeeder_zeeder-cache
docker compose -f compose.yml up -d
```

For systemd:

```bash
sudo systemctl stop zeeder
sudo rm -rf /var/cache/zeeder/zebra/network/*
sudo systemctl start zeeder
```

## Maintenance

### Logs

```bash
journalctl -u zeeder -f
docker compose -f compose.yml logs -f seeder
```

Each crawler tags its log lines with a `network` field, so you can filter for a
single network's crawl status.

### Restart

```bash
sudo systemctl restart zeeder
docker compose -f compose.yml restart seeder
```

### Upgrade

For systemd:

```bash
sudo systemctl stop zeeder
cd zeeder
git pull
cargo build --release
sudo install -m 0755 target/release/zeeder /opt/zeeder/zeeder
sudo systemctl start zeeder
```

For Docker:

```bash
docker compose -f compose.yml pull
docker compose -f compose.yml up -d
```

A Zebra network-upgrade release needs nothing beyond replacing the binary or
image. The [network upgrade runbook](network-upgrades.md) covers verification
and the expected servable-peer dip.

### Cache Persistence

- The systemd unit above sets `XDG_CACHE_HOME=/var/cache/zeeder`, so the peer
  caches live under `/var/cache/zeeder/zebra/network/`, one file per network.
- Bare-metal processes without `XDG_CACHE_HOME` use `~/.cache/zebra/network/`.
- Docker sets `XDG_CACHE_HOME=/cache`, so the caches live under
  `/cache/zebra/network/`.
- Persisting the caches speeds up restart, but deleting them is safe.

## Capacity Planning

Expected usage is small for normal DNS seeder traffic, scaled by the number of
networks served. A two-network process roughly doubles the peer connections and
P2P bandwidth of a single-network process:

- RAM: about 100 to 200 MB for two networks
- CPU: under 5% of one core
- Disk: under 20 MB for two peer caches
- Network: about 2 to 20 Mbps, depending on DNS query volume and P2P activity
- File descriptors: budget for roughly 200 peer sockets per network; raise the
  `ulimit` on hosts running multiple networks

Scale by adding independent seeder processes and publishing additional NS
records. Keep metrics and logs per process so one unhealthy instance does not
disappear behind aggregate fleet numbers.
