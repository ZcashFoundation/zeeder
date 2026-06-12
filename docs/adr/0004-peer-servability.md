# ADR 0004: Peer Servability and Protocol-Version Floor

## Status

Accepted

## Context

zebra-network's address book stores every peer it learns about, in every connection state, including `NeverAttemptedGossiped` addresses it has never contacted. `MetaAddr` carries no protocol version, so the seeder cannot filter served peers by version after the fact. The seeder also runs with no chain state, and zebra-network derives the handshake's minimum acceptable protocol version from the chain tip it is given.

## Decision

Serve a peer only when it is *servable*: recently handshaked (`was_recently_live`), advertising the full-node service (`NODE_NETWORK`), routable, on the network default port, not inbound, and carrying no zebra-network misbehavior score. Implement the decision once in `crawl::servability`. Leave banning to zebra-network, which removes a banned peer from the address book before the seeder classifies it. Replace `NoChainTip` with a `SeederChainTip` pinned to the current network upgrade's activation height, so zebra-network's handshake enforces that upgrade's protocol-version floor and outdated peers never reach the address book.

## Rationale

- A recent handshake transitively proves the peer passed the version floor and advertised the network service, so liveness and version-correctness are a single check.
- The floor is derived from zebra-chain's activation table, so it tracks future upgrades on a dependency bump rather than via a hardcoded constant.
- The seeder mirrors zebra-network's own GetAddr sanitization for peer provenance and quality: inbound-provenance peers and peers with non-zero misbehavior scores are not advertised to others.
- The seeder only enforces what DNS structurally requires: routable IP, default port, and address family, plus the same advertisement gates zebra-network applies to its own peer gossip.
- There is no second peer database and no active probing: zebra-network already crawls, handshakes, and tracks liveness, honoring ADR 0001.

## Consequences

- Outdated-version and unverified-gossip peers are no longer served, closing issue #19.
- Inbound-provenance and misbehaving peers are no longer advertised over DNS.
- One tested predicate owns the decision, and rejection reasons are exported via `zebra_seeder_peers_unservable{reason}`.
- The served set is smaller than the raw address book; on sparse networks such as testnet or IPv6, it can be thin. Watch `zebra_seeder_peers_servable`.
- The floor reaches the highest upgrade the pinned zebra activates, NU6.2 today. Full NU7 enforcement arrives when a future zebra release activates it. A tripwire test pins the expected floor.

## Revision History

- 2026-06-11: Added inbound and non-zero-misbehavior gates to match zebra-network's `MetaAddr::sanitize` advertisement policy.

## Alternatives Considered

- Active per-peer probing, as in sipa bitcoin-seeder: rejected because it duplicates zebra-network's crawling and contradicts ADR 0001.
- A configurable version override: rejected because the derived floor tracks upgrades automatically, while an override invites pointing nodes at the wrong fork.
