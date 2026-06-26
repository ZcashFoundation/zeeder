# zeeder Context

This glossary defines terms that carry architectural meaning in zeeder. Read it before changing peer servability, DNS behavior, metrics, or crawler integration.

## Servable

A peer is *servable* when the seeder may return it in DNS A or AAAA answers. A servable peer has been recently handshaked by zebra-network, advertises `NODE_NETWORK`, is routable, uses the network default port, was not learned from an inbound connection, and has no zebra-network misbehavior score. Servability is decided per network, against that network's crawler.

Raw address-book membership is not enough. Gossiped, stale, inbound-provenance, misbehaving, wrong-port, or non-full-node peers remain known to zebra-network but are not servable.

## Zone

A zone binds one Zcash network to its authoritative DNS identity: a `domain`, an out-of-zone `nameserver`, and a `ttl`. One process serves a set of zones, one per network, configured as a map keyed by network (`[zones.mainnet]`, `[zones.testnet]`). Each zone has its own crawler and its own servable-peer cache.

A query is routed to the zone whose domain contains the query name. Within the matched zone, exact-name A and AAAA queries return servable peers when that address family has entries, or NODATA with SOA when it is empty; exact-name SOA and NS queries return synthesized zone metadata; unsupported exact-name queries and deeper in-zone names return NODATA with SOA. A name outside every zone returns REFUSED. Zone domains must be disjoint so routing is unambiguous.

Changing zone routing or per-zone answer behavior is a DNS contract change. Update ADR 0002 and ADR 0005 and the DNS request-handler tests with any intentional change.

## Address-Family Split

The servable peer cache keeps IPv4 and IPv6 peers in separate lists. A queries read only the IPv4 list, and AAAA queries read only the IPv6 list. Each family is shuffled independently and capped independently, so a sparse IPv6 set does not reduce IPv4 answers.

Metrics use the `addr_family` label with the stable values `v4` and `v6`.

## Silent Drop

A silent drop is a rate-limit rejection where the seeder sends no DNS response. This avoids turning the seeder into an amplification source. Rate limiting is per client IP and shared across every zone: it runs before a query is routed to a zone. Do not replace silent drops with REFUSED or another DNS error unless ADR 0003 is changed deliberately.

Rate-limited packets increment `zeeder_dns_rate_limited_total`.

## Protocol-Version Floor

The protocol-version floor is the minimum peer protocol version zebra-network accepts during handshake. zeeder does not store peer protocol versions itself. Instead, each network's `SeederChainTip` pins zebra-network to that network's current activation height so outdated peers fail handshake before they can become servable.

The floor is reported per network through `zeeder_min_protocol_version` with a `network` label. Network-upgrade changes should update the chain-tip tests and ADR 0004.

## Contract Homes

- Configuration, metrics, deployment, and alerting live in `docs/operations.md`.
- Cross-component flow and DNS behavior live in `docs/architecture.md`.
- Durable architectural decisions live in `docs/adr/`. The multi-network topology lives in ADR 0005.
- Metric names and stable label values live in `src/metrics.rs`.
