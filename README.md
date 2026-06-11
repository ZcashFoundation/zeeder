# dnsseederNG

[![Rust CI](https://github.com/zcashfoundation/dnsseederNG/actions/workflows/ci.yml/badge.svg)](https://github.com/zcashfoundation/dnsseederNG/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/zcashfoundation/dnsseederNG/branch/dev/graph/badge.svg)](https://codecov.io/gh/zcashfoundation/dnsseederNG)

A Rust-based DNS seeder for the Zcash network, mirroring patterns from the [Zebra](https://github.com/zcashfoundation/zebra) project.

## Status
**Current State**: Beta.  Ready for production testing.

### Features
- **Project Structure**: Native rust seeder using `zebra-network` and `hickory-dns`.
- **Configuration**: Layered configuration system (Env Vars > Config File > Defaults) mirroring `zebrad`.
- **Dotenv Support**: Automatically loads configuration from a `.env` file if present.
- **CLI**: `clap`-based command line interface with `start` command.
- **Async Runtime**: Basic `tokio` orchestration with `tracing` for logging.
- **Crawler**: Active network crawler using `zebra-network`.
- **DNS Server**: Authoritative DNS server serving A/AAAA records from filtered peers using `hickory-dns`.
- **Rate Limiting**: Per-IP rate limiting to prevent DNS amplification attacks.
- **Testing**: Unit tests for configuration loading and CLI argument parsing. Integration tests for DNS server and crawler.

## Documentation

**📚 [Complete Documentation →](docs/)**

- **[Architecture](docs/architecture.md)** - System design, components, data flows, and architecture decisions
- **[Operations](docs/operations.md)** - Configuration, deployment, and monitoring guide
- **[Development](docs/development.md)** - Contributing and development guide

For team members reviewing the code, start with [Architecture](docs/architecture.md) to understand the design decisions.

## Usage

### Running the Seeder
```bash
cargo run start
```

### Verifying DNS Responses
Once the server is running, you can verify it using `dig`:

**IPv4 (A Record):**
```bash
dig @127.0.0.1 -p 1053 testnet.seeder.example.com A
```

**IPv6 (AAAA Record):**
```bash
dig @127.0.0.1 -p 1053 testnet.seeder.example.com AAAA
```

### Configuration
Configuration can be provided via a TOML file, environment variables, or a `.env` file.

**Environment Variables:**
Prefix with `ZEBRA_SEEDER__` (double underscore separator). 

**Using `.env` file:**
You can create a `.env` file in the project root to persist environment variables. See [`.env-example.txt`](.env-example.txt) for a template.

```bash
# Example .env content
ZEBRA_SEEDER__NETWORK__NETWORK="Mainnet"
# note: For production access, DNS server will need to be exposed on UDP/53, which is a privileged port.  Alternately, port forwarding can be used to forward production traffic to the seeder.
ZEBRA_SEEDER__DNS_LISTEN_ADDR="0.0.0.0:1053"
ZEBRA_SEEDER__DNS_TTL="600"
ZEBRA_SEEDER__SEED_DOMAIN="mainnet.seeder.example.com"
ZEBRA_SEEDER__METRICS__ENDPOINT_ADDR="0.0.0.0:9999"
```

### Configuration Parameters

| Parameter | Environment Variable | Default | Description |
|-----------|---------------------|---------|-------------|
| `dns_listen_addr` | `ZEBRA_SEEDER__DNS_LISTEN_ADDR` | `0.0.0.0:53` | DNS server listening address and port |
| `dns_ttl` | `ZEBRA_SEEDER__DNS_TTL` | `600` | DNS response TTL in seconds. Controls how long clients cache responses. Lower values (e.g., 300) provide fresher data but increase query load. Higher values (e.g., 1800) reduce load but slower updates. |
| `seed_domain` | `ZEBRA_SEEDER__SEED_DOMAIN` | `mainnet.seeder.example.com` | Domain name the seeder is authoritative for |
| `network.network` | `ZEBRA_SEEDER__NETWORK__NETWORK` | `Mainnet` | Zcash network to connect to (`Mainnet` or `Testnet`) |
| `metrics.endpoint_addr` | `ZEBRA_SEEDER__METRICS__ENDPOINT_ADDR` | (disabled) | Prometheus metrics endpoint address. Omit to disable metrics. |
| `rate_limit.queries_per_second` | `ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND` | `10` | Maximum DNS queries per second per IP address. Prevents DNS amplification attacks. |
| `rate_limit.burst_size` | `ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE` | `20` | Burst capacity for short traffic spikes (typically 2x the rate limit). |


## Architecture
- **Networking**: Uses `zebra-network` for peer discovery and management.
- **DNS Server**: Uses `hickory-dns` (formerly `trust-dns`) to serve DNS records.
- **Service Pattern**: Implements `tower::Service` for modular request handling.

## Metrics (Observability)

The seeder can expose Prometheus metrics. To enable them, uncomment this line in .env:

```bash
ZEBRA_SEEDER__METRICS__ENDPOINT_ADDR="0.0.0.0:9999"
```

Once enabled, metrics are available at `http://localhost:9999/metrics`.

### Key Metrics for Operators
Monitor these metrics to ensure the seeder is healthy and serving useful data:

-   **`seeder_peers_servable`** (Gauge, labels: `addr_family=v4|v6`): **Critical**. Peers the seeder will hand out: recently handshaked by zebra-network (so version-current and reachable), advertising the full-node service (`NODE_NETWORK`), routable, and on the default Zcash port. If this drops to 0, the seeder is returning empty lists.
-   **`seeder_peers_ineligible`** (Gauge, label: `reason`): Peers excluded this refresh, by reason (`not_routable`, `wrong_port`, `not_recently_live`, `not_full_node`). The dominant reason is normally `not_recently_live` (unverified gossip). Use it to explain a low servable count.
-   **`seeder_peers_known`** (Gauge): Raw size of the address book, including unverified and unreachable peers.
-   **`seeder_min_protocol_version`** (Gauge): The protocol-version floor the handshake enforces (for example `170150` for NU6.2). Confirms which network upgrade peers must meet.
-   **`seeder_build_info`** (Gauge = 1, labels: `version`, `network`): Build and network identification.
-   **`seeder_dns_queries_total`** (Counter, label: `record_type=A|AAAA`): Traffic volume.
-   **`seeder_dns_response_peers`** (Histogram): How many peers are returned per query. A healthy seeder returns near 25. A downward shift means the servable set is shrinking.
-   **`seeder_dns_rate_limited_total`** (Counter): **Important**. Queries blocked by rate limiting. High values may indicate an attack or legitimate clients being rate-limited (adjust limits if needed).
-   **`seeder_dns_errors_total`** (Counter): Should be near zero. Spikes indicate socket handling issues.
-   **`seeder_mutex_poisoning_total`** (Counter, label: `location=cache_updater|metrics_logger`): **Critical**. Should always be zero. Any non-zero value means a thread panicked while holding the address book lock. Investigate immediately and consider restarting the service.

## Deployment

### Docker (Recommended)
The project includes a `Dockerfile` and `docker-compose.yml` for easy deployment. The container uses a `rust` builder and a `distroless` runtime, minimal distroless image (Debian 13 "Trixie" based).

**Quick Start:**
```bash
docker-compose up -d
```
This starts the seeder on port `1053` (UDP/TCP).  Note that for production, this will need to be set to port `53` (UDP/TCP) or a reverse proxy or port forwarding rule will need to be added to forward traffic to port `1053`.

**Production Best Practices:**

1.  **Persistence**: Mount a volume for the address book cache to ensure peer data is retained across restarts.
    ```yaml
    volumes:
      - ./data:/root/.cache/zebra/network
    ```
2.  **Resource Limits**: The seeder is lightweight, but it is good practice to set limits.
    ```yaml
    deploy:
      resources:
        limits:
          cpus: '0.50'
          memory: 512M
    ```
3.  **Metrics**: Enable metrics in production for monitoring (see Metrics section).

**Manual Docker Build:**
```bash
docker build -t dnsseederNG .
docker run -d -p 1053:1053/udp -p 1053:1053/tcp dnsseederNG
```

**Configuration with Docker:**
Pass environment variables to the container. See `docker-compose.yml` for examples.

## License

This project is licensed under either of:

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.



