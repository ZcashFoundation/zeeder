# Architecture

## Overview

zebra-seeder is a DNS seeder for Zcash that crawls the network and serves DNS records pointing to healthy peers.

```mermaid
graph TD
    A[DNS Client] -->|UDP Query| B[hickory-dns Server]
    B --> C[Rate Limiter]
    C -->|Check IP| D{Allow?}
    D -->|Yes| E[DnsRequestHandler]
    D -->|No| F[Drop Packet]
    E --> G[Servable Peer Cache]
    G -->|watch channel| H[Cache Updater]
    H -->|5s interval| I[Address Book]
    I --> J[zebra-network]
    J -->|Peer Discovery| K[Zcash Network]
    E --> L[Return A/AAAA Records]
```

### Core Components

- **zebra-network**: Handles Zcash P2P networking and peer discovery
- **hickory-dns**: DNS server framework
- **Rate Limiter**: Per-IP rate limiting using governor crate
- **Servable Peer Cache**: Lock-free cache of servable peers, updated every 5 seconds
- **Address Book**: Thread-safe peer storage managed by zebra-network
- **Metrics**: Prometheus metrics via metrics-exporter-prometheus

## Data Flow

### Startup Sequence

1. Load configuration (defaults, optional TOML file, then environment overrides)
2. Initialize metrics endpoint (if enabled)
3. Bind DNS UDP and TCP sockets, failing before P2P startup if the configured address is unavailable
4. Initialize zebra-network with address book
5. Create rate limiter (if enabled)
6. Spawn address cache updater (updates every 5 seconds)
7. Register the pre-bound sockets and start DNS serving
8. Run until the DNS server exits, SIGINT is received, or SIGTERM is received

### DNS Query Handling

```mermaid
sequenceDiagram
    Client->>DNS Server: DNS query
    DNS Server->>Rate Limiter: Check IP limit
    alt Rate limit OK
        Rate Limiter->>DnsRequestHandler: Process query
        DnsRequestHandler->>DnsRequestHandler: Classify query name and type
        alt Exact seed name, A/AAAA
            DnsRequestHandler->>Servable Peer Cache: Read cached peers
            Note over DnsRequestHandler,Servable Peer Cache: Lock-free read via watch channel
            Servable Peer Cache-->>DnsRequestHandler: IPv4 or IPv6 peer list
            DnsRequestHandler-->>DNS Server: A/AAAA records
            DNS Server-->>Client: Response (up to 25 IPs)
        else Exact seed name, SOA/NS
            DnsRequestHandler-->>DNS Server: Zone metadata record
            DNS Server-->>Client: SOA or NS
        else In-zone non-apex or unsupported type
            DnsRequestHandler-->>DNS Server: NODATA + SOA
            DNS Server-->>Client: Empty NOERROR with SOA
        else Out of zone
            DnsRequestHandler-->>DNS Server: REFUSED
            DNS Server-->>Client: REFUSED
        end
    else Rate limited
        Rate Limiter-->>DNS Server: Drop
        Note over Client,DNS Server: No response (silent drop)
    end
```

**Steps:**
1. Client sends DNS query
2. Rate limiter checks if IP is within limits
3. If rate-limited: packet dropped silently (no amplification)
4. If allowed: classify the query against `dns.domain`
5. For exact `dns.domain` A/AAAA queries, read cached addresses (lock-free via watch channel)
6. Return pre-filtered and shuffled peers, or static SOA/NS metadata for SOA/NS queries
7. Return NODATA plus SOA for unsupported exact-name queries or deeper in-zone labels
8. Return REFUSED for names outside the configured seed domain

### Address Cache Updates

```mermaid
sequenceDiagram
    participant CacheUpdater
    participant AddressBook
    participant AddressCache
    participant Metrics
    
    loop Every 5 seconds
        CacheUpdater->>AddressBook: Lock & read peers
        AddressBook-->>CacheUpdater: All peers
        CacheUpdater->>CacheUpdater: Classify (recently-live, full node, routable, default port, outbound, clean)
        CacheUpdater->>CacheUpdater: Separate IPv4/IPv6
        CacheUpdater->>CacheUpdater: Shuffle & take 25 each
        CacheUpdater->>AddressCache: Update via watch channel
        Note over AddressCache: DNS queries read from here
    end
```

**How it works:**
- Background task updates cache every 5 seconds
- Cache contains pre-filtered, pre-shuffled IPv4 and IPv6 lists
- DNS queries read from cache without locking (via tokio `watch` channel)
- Eliminates lock contention during high query load

### Crawler Status and Metrics

```mermaid
sequenceDiagram
    participant CacheUpdater
    participant AddressBook
    participant Metrics
    
    loop Every 5 seconds
        CacheUpdater->>AddressBook: Lock & classify peers
        AddressBook-->>CacheUpdater: Peer metadata
        CacheUpdater->>Metrics: Update peer gauges
        Note over CacheUpdater: Logs crawler status every 10 minutes
    end
```

**How it works:**
- zebra-network continuously discovers and manages peers
- The address cache updater publishes peer count gauges from the same classification pass that refreshes DNS answers
- The updater logs crawler status every 10 minutes without taking an extra address-book lock

## Components Deep Dive

### zebra-network Integration

**What:** Zcash P2P networking library from the Zebra project

**Responsibilities:**
- Peer discovery via DNS seeds and peer exchange
- Connection management
- Protocol message handling
- Address book maintenance

**Our usage:**
- Initialize with config and a dummy inbound service (reject all)
- Pass a `SeederChainTip` pinned to the current network upgrade, so the handshake rejects outdated-version peers
- Read the Address Book for servable peers
- Never send blockchain data (we're not a full node)

---

### hickory-dns Server

**What:** Async DNS server framework (formerly trust-dns)

**Responsibilities:**
- DNS protocol handling (UDP/TCP)
- Query parsing
- Response building

**Our usage:**
- Implement `RequestHandler` trait via `DnsRequestHandler`
- Handle A, AAAA, SOA, and NS queries at the configured seed name
- Return NODATA plus SOA for unsupported exact-name queries and deeper in-zone labels
- Return REFUSED for unauthorized domains

---

### Rate Limiter

**What:** Per-IP rate limiting to prevent DDoS amplification

**Implementation:**
- `governor` crate with token bucket algorithm
- `DashMap` for concurrent IP tracking
- Each IP gets isolated rate limiter instance

**Configuration:**
- Default: 10 queries/second per IP
- Burst: 20 queries (2x rate)
- Configurable via `rate_limit` config section

**Behavior:**
- Rate-limited requests: dropped silently (no response)
- Metric: `zebra_seeder_dns_rate_limited_total`

---

### Address Cache

**What:** Lock-free cache providing pre-filtered peer addresses for DNS responses

**Problem Solved:**
- Original design locked the address book mutex on every DNS query
- Under high query load, this caused lock contention
- Queries backed up waiting for the mutex

**Solution:**
- Background task updates cache every 5 seconds
- Uses `tokio::sync::watch` channel for lock-free reads
- DNS queries read from cache without any locking

**Behavior:**
- Cache refresh takes one address-book lock every 5 seconds.
- Each refresh classifies peers, separates IPv4 and IPv6 addresses, shuffles each family, and caps each answer set at 25.
- DNS handlers read the current snapshot through a watch channel without taking the address-book lock.

**Trade-offs:**
- ✅ Zero lock contention during DNS queries
- ✅ Predictable low-latency responses
- ⚠️ Peer list may be up to 5 seconds stale (acceptable for DNS seeding)

---

### Address Book & Mutex Handling

**Mutex Strategy:**
- Address Book protected by `std::sync::Mutex`
- Only locked by cache updater (every 5 seconds)
- DNS queries never lock the mutex directly

**Poisoning Recovery:**
- If thread panics while holding lock, mutex becomes "poisoned"
- We recover by calling `poisoned.into_inner()`
- Log error + increment metric
- Continue serving (availability over strict correctness)

**Peer Servability (done in cache updater, see `crawl::servability`):**

A peer is *servable* only if it is:
- Recently live: zebra-network handshaked it within the liveness window (transitively current-version and reachable)
- A full node: advertises the `NODE_NETWORK` service (zebra's handshake enforces the version floor but not services, so the seeder gates on it)
- Routable (no loopback, unspecified, multicast)
- On the network default port (usually 8233)
- Not recorded from an inbound connection
- Clean: no zebra-network misbehavior score

The inbound and misbehavior gates mirror zebra-network's own `MetaAddr::sanitize` behavior for GetAddr replies. A peer that reaches the ban threshold is removed from the address book in the same update that bans it, so it never reaches this filter; a peer with a sub-ban misbehavior score remains in the address book but is not served over DNS.

Servable peers are then separated by address family (IPv4/IPv6), shuffled, and capped at 25.

---

### Configuration System

**Override priority:**
1. Environment variables (`ZEBRA_SEEDER__*`)
2. TOML config file
3. Hardcoded defaults

**Implementation:**
- `config` crate for loading
- `serde` for deserialization
- Optional `.env` loading via `dotenvy`; a present malformed `.env` fails startup
- Validation runs on the resolved config before `print-config` or DNS startup proceeds

---

### Metrics

Prometheus metrics are exposed on the configured metrics endpoint. Architecture-level metrics are grouped around peer servability, DNS traffic, rate limiting, mutex poisoning, protocol-version floor, and build identity. The canonical metric reference, labels, and alert guidance live in [Operations](operations.md#metrics).

## Architecture Decision Records

- [ADR 0001: Use zebra-network for Peer Discovery](adr/0001-zebra-network.md)
- [ADR 0002: Use hickory-dns for DNS Server](adr/0002-hickory-dns.md)
- [ADR 0003: Implement Per-IP Rate Limiting](adr/0003-rate-limiting.md)
- [ADR 0004: Peer Servability and Protocol-Version Floor](adr/0004-peer-servability.md)

## Design Principles

1. **Security First**: Rate limiting and domain validation prevent abuse
2. **Availability**: Mutex poisoning recovery ensures continued operation
3. **Performance**: Concurrent data structures, early limiting, minimal allocations
4. **Observability**: Comprehensive metrics for monitoring
5. **Simplicity**: Leverage proven libraries (zebra-network, hickory-dns)
6. **Configurability**: All key parameters are configurable
