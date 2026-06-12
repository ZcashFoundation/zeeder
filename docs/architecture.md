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

1. Load configuration (env vars → TOML → defaults)
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

**Files:** `src/seeder.rs` (initialization), `src/crawl/chain_tip.rs` (protocol-version floor)

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

**Files:** `src/dns/request_handler.rs`

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
- Metric: `seeder_dns_rate_limited_total`

**Files:** `src/dns/rate_limiter.rs`, `src/config.rs` (`RateLimitConfig`)

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

**Implementation:**
```rust
struct ServablePeers {
    ipv4: Vec<PeerSocketAddr>,  // Pre-filtered, shuffled
    ipv6: Vec<PeerSocketAddr>,  // Pre-filtered, shuffled
}

// Cache updater runs every 5 seconds:
// 1. Lock address book
// 2. Classify peers (crawl::servability); keep servable ones
// 3. Shuffle and take 25 each (per address family)
// 4. Send to watch channel

// DNS handler reads without locking:
let peers = match record_type {
    A => cache.borrow().ipv4.clone(),
    AAAA => cache.borrow().ipv6.clone(),
};
```

**Trade-offs:**
- ✅ Zero lock contention during DNS queries
- ✅ Predictable low-latency responses
- ⚠️ Peer list may be up to 5 seconds stale (acceptable for DNS seeding)

**Files:** `src/crawl/address_cache.rs`

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

**Layered priority:**
1. Environment variables (`ZEBRA_SEEDER__*`)
2. TOML config file
3. Hardcoded defaults

**Implementation:**
- `config` crate for loading
- `serde` for deserialization
- `.env` file support via `dotenvy`

**Files:** `src/config.rs`, `src/commands.rs`

---

### Metrics

**Prometheus metrics exposed on configurable endpoint (default `:9999/metrics`)**

**Key metrics:**
- `seeder_peers_known` - Total peers in the address book (gauge)
- `seeder_peers_servable` - Servable peers by address family (gauge, `addr_family`)
- `seeder_peers_unservable` - Excluded peers by reason (gauge, `reason`)
- `seeder_min_protocol_version` - Enforced protocol-version floor (gauge)
- `seeder_build_info` - Build and network identification (gauge, `version`/`git_sha`/`network`)
- `seeder_dns_queries_total` - DNS queries by record type (counter)
- `seeder_dns_response_peers` - Peers per response (histogram)
- `seeder_dns_rate_limited_total` - Rate-limited queries (counter)
- `seeder_dns_errors_total` - DNS errors (counter)
- `seeder_mutex_poisoning_total` - Mutex poisoning events (counter)

**Files:** `src/metrics.rs`, `src/seeder.rs`, `src/crawl/address_cache.rs`, `src/dns/request_handler.rs`

## Architecture Decision Records

### ADR 001: Use zebra-network for Peer Discovery

**Status:** Accepted

**Context:**  
We need to crawl the Zcash network to discover and maintain a list of healthy peers.

**Decision:**  
Use the `zebra-network` crate instead of implementing custom P2P networking.

**Rationale:**
- **Proven**: Battle-tested in Zebra full node
- **Avoids duplication**: Complex P2P logic already implemented
- **Protocol compatibility**: Follows Zcash protocol exactly
- **Maintenance**: Benefits from ongoing Zebra improvements
- **Reduced bugs**: Don't recreate peer discovery, connection management, etc.

**Consequences:**
- ✅ Faster development
- ✅ Reduced bug surface
- ✅ Protocol compliance guaranteed
- ⚠️ Dependency on zebra-network versions
- ⚠️ Must track Zebra releases for updates

**Alternatives Considered:**
- Custom P2P implementation - Rejected (too complex, high bug risk)
- libp2p - Rejected (incompatible with Zcash protocol)

---

### ADR 002: Use hickory-dns for DNS Server

**Status:** Accepted

**Context:**  
We need to serve DNS A/AAAA records to clients querying for Zcash peers.

**Decision:**  
Use the `hickory-dns` crate (formerly trust-dns) for DNS serving. The seeder is authoritative for the exact configured `dns.domain`: A/AAAA queries return servable peers, SOA/NS queries return synthesized zone metadata, unsupported exact-name queries return NODATA with SOA, deeper in-zone names return NODATA with SOA, and out-of-zone names return REFUSED.

**Rationale:**
- **Mature**: Industry-standard Rust DNS implementation
- **RFC compliant**: Handles DNS protocol complexities correctly
- **Async native**: Works with tokio ecosystem
- **Feature-rich**: Supports all DNS record types we need
- **Tower integration**: Modern request/response abstraction

**Consequences:**
- ✅ Correct DNS protocol handling
- ✅ Good performance
- ✅ Well-maintained
- ✅ Negative answers are cacheable because NODATA responses include SOA.
- ✅ Subdomain queries do not expand the peer-serving surface.
- ⚠️ Additional dependency
- ⚠️ Learning curve for API

**Revision History:**
- 2026-06-11: Completed the authoritative DNS contract: exact seed-name matching, SOA/NS answers, and SOA-backed NODATA responses.

**Alternatives Considered:**
- Custom DNS parser - Rejected (too complex, error-prone, RFC compliance burden)
- trust-dns (old name) - N/A (hickory-dns is the successor)

---

### ADR 003: Implement Per-IP Rate Limiting

**Status:** Accepted

**Context:**  
DNS seeders on UDP port 53 are vulnerable to DNS amplification attacks where:
- Attackers forge source IP addresses
- Small queries trigger large responses
- Our seeder becomes a DDoS weapon

**Decision:**  
Implement per-IP rate limiting using `governor` crate:
- 10 queries/second per IP (default, configurable)
- Burst capacity of 20
- Silent packet dropping (no REFUSED response)

**Rationale:**
- **Security**: Prevents amplification attacks
- **Fairness**: No single IP can monopolize resources
- **Performance**: <1ms overhead with DashMap
- **Configurability**: Operators can tune based on traffic
- **Silent drops**: Avoid amplification (no error responses)

**Implementation:**
- `governor` crate for token bucket algorithm (GCRA)
- `DashMap` for per-IP tracking across threads in a single map
- Each IP gets isolated rate limiter instance
- Metrics track rate-limited requests

**Consequences:**
- ✅ Cannot be weaponized for DDoS
- ✅ Fair resource allocation
- ✅ Minimal performance impact
- ⚠️ Legitimate high-volume clients may be rate-limited
- ⚠️ Memory grows with unique IPs (acceptable: ~200 bytes/IP)

**Alternatives Considered:**
- No rate limiting - Rejected (unacceptable security risk)
- Global rate limit - Rejected (single client could DoS all others)
- Response size limiting - Rejected (insufficient, still allows amplification)

---

### ADR 004: Peer Servability and Protocol-Version Floor

**Status:** Accepted

**Context:**
zebra-network's address book stores every peer it learns about, in every connection state, including `NeverAttemptedGossiped` addresses it has never contacted. `MetaAddr` carries no protocol version, so the seeder cannot filter served peers by version after the fact. The seeder also runs with no chain state, and zebra-network derives the handshake's minimum acceptable protocol version from the chain tip it is given.

**Decision:**
Serve a peer only when it is *servable*: recently handshaked (`was_recently_live`), advertising the full-node service (`NODE_NETWORK`), routable, on the network default port, not inbound, and carrying no zebra-network misbehavior score. Implement the decision once in `crawl::servability`. Leave banning to zebra-network, which removes a banned peer from the address book before the seeder classifies it. Replace `NoChainTip` with a `SeederChainTip` pinned to the current network upgrade's activation height, so zebra-network's handshake enforces that upgrade's protocol-version floor and outdated peers never reach the address book.

**Rationale:**
- A recent handshake transitively proves the peer passed the version floor and advertised the network service, so liveness and version-correctness are a single check.
- The floor is derived from zebra-chain's activation table, so it tracks future upgrades on a dependency bump rather than via a hardcoded constant.
- The seeder mirrors zebra-network's own GetAddr sanitization for peer provenance and quality: inbound-provenance peers and peers with non-zero misbehavior scores are not advertised to others.
- The seeder only enforces what DNS structurally requires (routable IP, default port, address family) plus the same advertisement gates zebra-network applies to its own peer gossip.
- No second peer database and no active probing: zebra-network already crawls, handshakes, and tracks liveness (honoring ADR 001).

**Consequences:**
- ✅ Outdated-version and unverified-gossip peers are no longer served (issue #19).
- ✅ Inbound-provenance and misbehaving peers are no longer advertised over DNS.
- ✅ One tested predicate; rejection reasons are exported via `seeder_peers_unservable{reason}`.
- ⚠️ The served set is smaller than the raw address book; on sparse networks (testnet, IPv6) it can be thin. Watch `seeder_peers_servable`.
- ⚠️ The floor reaches the highest upgrade the pinned zebra activates (NU6.2 today); full NU7 enforcement arrives when a future zebra release activates it. A tripwire test pins the expected floor.

**Revision History:**
- 2026-06-11: Added inbound and non-zero-misbehavior gates to match zebra-network's `MetaAddr::sanitize` advertisement policy.

**Alternatives Considered:**
- Active per-peer probing (sipa bitcoin-seeder style) - Rejected: duplicates zebra-network's crawling and contradicts ADR 001.
- A configurable version override - Rejected: the derived floor tracks upgrades automatically; an override invites pointing nodes at the wrong fork.

## Design Principles

1. **Security First**: Rate limiting and domain validation prevent abuse
2. **Availability**: Mutex poisoning recovery ensures continued operation
3. **Performance**: Concurrent data structures, early limiting, minimal allocations
4. **Observability**: Comprehensive metrics for monitoring
5. **Simplicity**: Leverage proven libraries (zebra-network, hickory-dns)
6. **Configurability**: All key parameters are configurable
