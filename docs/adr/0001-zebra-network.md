# ADR 0001: Use zebra-network for Peer Discovery

## Status

Accepted

## Context

We need to crawl the Zcash network to discover and maintain a list of healthy peers.

## Decision

Use the `zebra-network` crate instead of implementing custom P2P networking.

## Rationale

- **Proven**: Battle-tested in Zebra full node
- **Avoids duplication**: Complex P2P logic already implemented
- **Protocol compatibility**: Follows Zcash protocol exactly
- **Maintenance**: Benefits from ongoing Zebra improvements
- **Reduced bugs**: Do not recreate peer discovery, connection management, or related P2P behavior

## Consequences

- Faster development
- Reduced bug surface
- Protocol compliance guaranteed
- Dependency on zebra-network versions
- Must track Zebra releases for updates

## Revision History

- 2026-06-26: One process runs an independent `zebra_network::init` per served network (see ADR 0005). Each crawler keeps its state behind the values `init` returns; the composition root holds one peer-set handle per network for the process lifetime. Peer cache files are network-keyed, so concurrent crawlers do not collide.

## Alternatives Considered

- Custom P2P implementation: rejected because it is too complex and has high bug risk
- libp2p: rejected because it is incompatible with the Zcash protocol
