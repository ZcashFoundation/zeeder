# ADR 0004: Peer Servability and Protocol-Version Floor

## Status

Accepted

## Context

zebra-network's address book stores every peer it learns about, in every connection state, including `NeverAttemptedGossiped` addresses it has never contacted. `MetaAddr` carries a negotiated protocol version only after a successful handshake. The seeder also runs with no chain state, and zebra-network derives the handshake's minimum acceptable protocol version from the chain tip it is given.

## Decision

Serve a peer only when it is *servable*: recently handshaked (`was_recently_live`), negotiated at or above the current protocol-version floor, advertising the full-node service (`NODE_NETWORK`), routable, on the network default port, not inbound, and carrying no zebra-network misbehavior score. Implement the decision once in `crawl::servability`. Leave banning to zebra-network, which removes a banned peer from the address book before the seeder classifies it. Supply zebra-network with the observed `SeederChainTip` defined by [ADR 0007](0007-observed-network-upgrade-activation.md), so the handshake and served-address cache use the same dynamic floor.

## Rationale

- A recent handshake proves reachability and records the negotiated version, while an explicit version check removes entries admitted before a dynamic floor change.
- The floor target comes from zebra-chain's activation table, and ADR 0007 separates that compiled target from the observed activation decision.
- The seeder mirrors zebra-network's own GetAddr sanitization for peer provenance and quality: inbound-provenance peers and peers with non-zero misbehavior scores are not advertised to others.
- The seeder only enforces what DNS structurally requires: routable IP, default port, and address family, plus the same advertisement gates zebra-network applies to its own peer gossip.
- There is no second peer database. The activation observer performs bounded isolated handshakes against peers selected from zebra-network's address book, while zebra-network retains ownership of discovery and liveness.

## Consequences

- Outdated-version and unverified-gossip peers are not served, closing issue #19.
- Inbound-provenance and misbehaving peers are no longer advertised over DNS.
- One tested predicate owns the decision, and rejection reasons are exported via `zeeder_peers_unservable{reason}`.
- The served set is smaller than the raw address book; on sparse networks such as testnet or IPv6, it can be thin. Watch `zeeder_peers_servable`.
- The floor remains at the previous upgrade until ADR 0007's observer confirms the newest compiled target. The runbook in `docs/network-upgrades.md` owns the dependency bump and operational procedure.

## Revision History

- 2026-06-11: Added inbound and non-zero-misbehavior gates to match zebra-network's `MetaAddr::sanitize` advertisement policy.
- 2026-07-03: Floors are pinned per network by one tripwire test each; the network-upgrade runbook owns the bump procedure.
- 2026-07-21: ADR 0007 replaced deploy-time floor changes with observed activation and added explicit reclassification of negotiated versions.

## Alternatives Considered

- Probing every address independently: rejected because it duplicates zebra-network's crawler. ADR 0007 limits extra probes to 1 recently live peer per network group during activation observation.
- A configurable version override: rejected because the derived floor tracks upgrades automatically, while an override invites pointing nodes at the wrong fork.
