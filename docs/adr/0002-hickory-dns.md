# ADR 0002: Use Hickory DNS for DNS Server

## Status

Accepted

## Context

We need to serve DNS A/AAAA records to clients querying for Zcash peers.

## Decision

Use Hickory DNS's `hickory-server` and `hickory-proto` crates, formerly trust-dns, for DNS serving. The seeder is authoritative for the exact configured `dns.domain`: A/AAAA queries return servable peers when that address family has entries, empty A/AAAA families return NODATA with SOA, SOA/NS queries return configured zone metadata, unsupported exact-name queries return NODATA with SOA, deeper in-zone names return NODATA with SOA, and out-of-zone names return REFUSED.

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

## Alternatives Considered

- Custom DNS parser: rejected because it is too complex, error-prone, and creates an RFC-compliance burden
- trust-dns: not applicable because Hickory DNS is the successor
