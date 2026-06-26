# ADR 0002: Use Hickory DNS for DNS Server

## Status

Accepted

## Context

We need to serve DNS A/AAAA records to clients querying for Zcash peers.

## Decision

Use Hickory DNS's `hickory-server` and `hickory-proto` crates, formerly trust-dns, for DNS serving. A single `DnsRequestHandler` is authoritative for every configured zone and routes each query to the zone whose domain contains the query name. Within the matched zone: A/AAAA queries return servable peers when that address family has entries, empty A/AAAA families return NODATA with SOA, SOA/NS queries return configured zone metadata, unsupported exact-name queries return NODATA with SOA, and deeper in-zone names return NODATA with SOA. A name outside every configured zone returns REFUSED. Configuration rejects overlapping zone domains, so at most one zone matches any query.

Zeeder does not adopt Hickory's `Catalog` or `ZoneHandler` abstractions. They model a record store (AXFR, NXDOMAIN proofs, DNSSEC) that a synthetic seeder does not use, and they offer no hook for the silent rate-limit drop (see ADR 0003). The custom handler couples only to `RequestHandler`, `ResponseHandler`, and `MessageResponseBuilder`.

## Rationale

- **Mature**: Industry-standard Rust DNS implementation
- **RFC compliant**: Handles DNS protocol complexities correctly
- **Async native**: Works with tokio ecosystem
- **Feature-rich**: Supports all DNS record types we need
- **Tower integration**: Modern request/response abstraction

## Consequences

- Correct DNS protocol handling
- Good performance
- Well-maintained dependency
- Negative answers are cacheable because NODATA responses include SOA
- Subdomain queries do not expand the peer-serving surface
- The configured authoritative nameserver must be outside `dns.domain` because Zeeder does not serve glue or address records for nameserver hostnames
- Additional dependency
- Learning curve for API

## Revision History

- 2026-06-11: Completed the authoritative DNS contract: exact seed-name matching, SOA/NS answers, and SOA-backed NODATA responses.
- 2026-06-12: Replaced synthesized in-zone NS metadata with an explicit out-of-zone `dns.nameserver` setting.
- 2026-06-26: Generalized the handler from one zone to a routed zone set, one per network, on a shared listener. REFUSED now applies only to names outside every zone. Recorded the decision not to adopt Hickory's `Catalog`/`ZoneHandler`. See ADR 0005 for the multi-network topology.

## Alternatives Considered

- Custom DNS parser: rejected because it is too complex, error-prone, and creates an RFC-compliance burden
- trust-dns: not applicable because Hickory DNS is the successor
