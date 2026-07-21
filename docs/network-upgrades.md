# Network Upgrade Runbook

Zeeder can be deployed before a network upgrade without dropping nodes that remain valid under the previous protocol version. A dependency bump supplies a new activation target; the running seeder raises its protocol floor only after its own diverse peer observations show that the activation is safely buried.

## Activation Model

Each network crawler starts immediately below the newest activation height compiled into zebra-chain, which keeps zebra-network's handshake at the previous protocol-version floor. The activation observer then samples recently live peers from Zeeder's own address book through isolated Zcash handshakes. It does not query a configured node, remote procedure call endpoint, block explorer, or public API.

The floor advances only after all of these conditions hold:

- At most 64 available IPv4 `/16` or IPv6 `/32` network groups are selected uniformly, with 1 peer sampled from each selected group.
- At least 12 groups participate.
- At least 75% of those groups report the activation height plus Zebra's maximum reorganization depth, negotiate the target protocol version, and advertise `NODE_NETWORK`.
- The same threshold holds for 3 consecutive sweeps, separated by the target block spacing.

Failed handshakes and timeouts count as not ready, and any nonqualifying sweep resets the consecutive-sweep count. The algorithm does not use a maximum height, average height, decaying threshold, or wall-clock activation date. Peer heights are self-reported, so the network-group quorum limits raw-address Sybil weight but does not eliminate the risk of a group-supermajority attack or a complete eclipse.

Before it raises the floor, Zeeder atomically writes an activation record beside zebra-network's peer cache. On restart, Zeeder accepts only a record that exactly matches the compiled activation height, confirmation height, and protocol version; a future dependency bump therefore requires fresh evidence for its new target.

## Maintainer Procedure

Trigger this procedure when a Zebra release sets an upgrade activation height for any supported network.

1. Read the Zebra release notes, and note the activation height, affected network, and minimum peer protocol version.
2. Bump `zebra-network` and `zebra-chain` in `Cargo.toml` to the versions published with that release.
3. Run `./commit_checks.sh`. The target tests in `src/crawl/activation.rs` fail when the compiled activation details change.
4. Verify the new expected activation height, confirmation height, pre-activation floor, and target protocol version against Zebra, then update the explicit test values. Do not compute those expected values from the dependency.
5. Fix Zebra API changes, and update any documentation invalidated by the release.
6. Release Zeeder through the [release process](development.md#release-process).

Deploying promptly gives the observer time to populate a diverse address book before activation. Deployment does not itself remove peers on the previous protocol version.

## Operator Procedure

Replace the binary or image while preserving the cache volume:

- Docker: pull the release image, then run `docker compose up -d` to recreate the container.
- systemd: build and install the release binary, then restart the unit by following the [upgrade steps](operations.md#upgrade).

No network-upgrade configuration is required. Zones, nameservers, time to live values, DNS delegation, listener ports, and rate limits remain independent of the activation target.

### Verify the Floor

```bash
curl -s http://127.0.0.1:9999/metrics | grep -E 'zeeder_min_protocol_version|zeeder_peers_servable|zeeder_peers_unservable'
```

Before observer confirmation, `zeeder_min_protocol_version` reports the previous upgrade's floor, and nodes valid under that floor remain eligible for DNS. Each observation sweep also writes an `activation observation sweep` log with its total groups, ready groups, consecutive qualifying sweeps, and target values.

After the confirmation height and quorum requirements are met, the floor metric moves to the new protocol version. The next 5-second address-cache refresh removes already-handshaked peers below the new floor from DNS, while zebra-network rejects new outdated handshakes.

The transition may happen later than the confirmation height when fewer than 12 network groups are available or less than 75% qualify. That delay is a conservative lack of evidence, not a reason to lower the threshold.

### Persistence and Recovery

The confirmation record shares zebra-network's cache root:

| Environment | Example mainnet record |
|-------------|------------------------|
| Docker with `XDG_CACHE_HOME=/cache` | `/cache/zebra/network/mainnet.activation` |
| systemd with `XDG_CACHE_HOME=/var/cache/zeeder` | `/var/cache/zeeder/zebra/network/mainnet.activation` |
| Default user cache | `~/.cache/zebra/network/mainnet.activation` |

Keep the cache volume during ordinary upgrades and rolls. If the record is absent, mismatched, or unreadable, Zeeder keeps the previous floor and observes the target again. If the cache is disabled or the record cannot be written, Zeeder refuses to raise the floor because it cannot make the decision durable.

To force fresh observation after investigating a suspected false confirmation, stop Zeeder, delete only the affected network's `mainnet.activation` or `testnet.activation` file, and restart. Do not clear the peer cache, because its recently discovered addresses provide the independent observation set.

## Quick Reference

| Question | Answer |
|----------|--------|
| Does an upgrade require Zeeder configuration changes? | No |
| Can the new image be deployed before activation? | Yes; deployment keeps the previous floor |
| What causes the floor to rise? | 75% of a uniform sample of 12 to 64 network groups qualifying across 3 consecutive sweeps after the confirmation height |
| Does Zeeder depend on a node or endpoint? | No; each instance observes peers from its own address book |
| Must the peer cache be cleared? | No; preserve it for observation and restart continuity |
| What is the recovery control? | Delete only the affected `.activation` record, then restart |
