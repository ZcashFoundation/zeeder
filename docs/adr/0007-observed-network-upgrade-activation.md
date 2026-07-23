# ADR 0007: Observed Network-Upgrade Activation

## Status

Accepted

## Context

zebra-network derives its minimum peer protocol version from the `ChainTip` that Zeeder supplies. Zeeder has no local chain state because it is a crawler, not a validating node, so a dependency bump cannot tell it whether the network has reached a compiled activation height. Raising the synthetic tip during deployment would reject nodes that are still valid before activation, while relying on one remote procedure call endpoint would make an otherwise independent seeder depend on one node.

Peers already report their start height and protocol version during the Zcash handshake. Those claims are unauthenticated, but Zeeder can combine them with its own address book, recent-handshake filter, and network diversity to make a conservative activation decision without adding a trusted endpoint.

## Decision

Each crawler starts its `SeederChainTip` immediately below the newest compiled activation height. It raises the tip to that activation height only after an independent observer confirms all of these conditions:

- The observer uniformly samples at most 64 available IPv4 `/16` or IPv6 `/32` network groups, then chooses 1 recently live, outbound, full node from each selected group.
- At least 12 network groups participate in a completed sweep.
- At least 75% of the sampled groups report a start height at or above the activation height plus Zebra's maximum reorganization depth, negotiate the new protocol version, and advertise `NODE_NETWORK`.
- The threshold holds for 3 consecutive sweeps, separated by the target block spacing. A timeout, failed handshake, or nonqualifying response remains in the denominator and counts as not ready.

The observer uses isolated Zcash handshakes whose floor remains at the previous upgrade, which lets it measure both old-version and new-version nodes. It does not use the maximum height, an average height, a decaying threshold, an external application programming interface (API), or a designated node.

Before raising the floor, Zeeder atomically persists an exact record of the activation height, confirmation height, and required protocol version beside zebra-network's peer cache. A restart accepts only a record that exactly matches the compiled target. If the cache is disabled or the record cannot be written, the observer leaves the previous floor in place.

## Rationale

Network-group voting limits the weight of many addresses from one prefix, while uniform selection prevents prefixes containing more addresses from gaining extra sampling weight. The 64-group cap bounds concurrent handshakes and prevents an attacker-influenced address book from expanding the quorum denominator. The minimum group count prevents a small, internally consistent view from deciding activation, and a fixed 75% threshold requires a supermajority without allowing a stalled minority to block the transition indefinitely. Requiring 3 spaced sweeps rejects brief height spikes and transient partitions, while waiting through the maximum reorganization depth avoids reacting at the activation boundary.

The algorithm treats missing evidence conservatively. Failed and timed-out probes do not disappear from the denominator, and any nonqualifying sweep resets the consecutive-sweep counter. Persistence makes the transition monotonic across ordinary restarts and fleet rolls.

## Consequences

- Zeeder can be deployed before activation without removing nodes that satisfy the previous protocol floor.
- Each Zeeder instance decides independently from the peers it has discovered, so the design adds no node or endpoint dependency.
- Each observation sweep opens at most 64 concurrent isolated handshakes.
- The protocol floor can rise later than the chain reaches the confirmation height when the address book lacks 12 groups or fewer than 75% of groups qualify.
- After confirmation, the servable-peer cache rechecks each peer's negotiated version against the new floor, which removes handshakes admitted under the previous floor from DNS responses immediately.
- Peer start heights remain self-reported. An attacker that controls at least 75% of the sampled network groups, or fully eclipses a seeder, can still cause a false confirmation; this design raises the cost of false evidence but cannot authenticate chain work.
- Operators must persist the cache directory for confirmation to survive a restart. Deleting only the network's `.activation` file and restarting forces a fresh observation without clearing discovered peers.

## Alternatives Considered

- Raise the floor when the new binary starts: rejected because deployment time is not activation time.
- Query a configured node or public endpoint: rejected because it introduces a single dependency and transfers trust to that endpoint.
- Use the maximum observed height: rejected because 1 false report can decide the transition.
- Use the arithmetic mean: rejected because outliers distort it and raw address counts give Sybil addresses excessive weight.
- Wait for every peer: rejected because stale, stalled, or abandoned nodes could prevent activation forever.
- Use wall-clock time alone: rejected because network activation is height-based and block production can vary.
