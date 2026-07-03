# Network Upgrade Runbook

What to do in Zeeder when a Zebra release ships a Zcash network upgrade.

## How Zeeder Reacts To A Network Upgrade

Zeeder enforces a per-network protocol-version floor during the P2P handshake
([ADR 0004](adr/0004-peer-servability.md)). Each network's crawler pins its
chain tip at the activation height of the highest upgrade in zebra-chain's
compiled activation table. zebra-network rejects handshakes below that
upgrade's minimum protocol version, so outdated peers never become servable.

Two properties drive this runbook:

- The floor is compiled in. It moves only when Zeeder is rebuilt against Zebra
  crates whose activation table contains the new height. No configuration key
  or DNS setting affects it.
- The floor moves at deploy time, not at activation time. A rebuilt seeder
  immediately serves only peers whose node release already supports the
  upgrade, which is what a bootstrapping node needs near an activation.

A Zcash upgrade usually spans two Zebra releases. An early release sets the
testnet activation height, and a later one sets mainnet. Each release runs
this runbook once, for the network it activates. A release that only scaffolds
a future upgrade, without an activation height, moves no floor and needs only
a routine dependency bump.

## Maintainer Procedure

Trigger: a Zebra release sets an upgrade activation height for any network.

1. Read the Zebra release notes. Note which network gained an activation
   height.
2. Bump `zebra-network` and `zebra-chain` in `Cargo.toml` to the versions
   published with that release.
3. Run `./commit_checks.sh`. The tripwire tests in `src/crawl/chain_tip.rs`
   fail for each network whose floor moved, and the assertion output shows the
   new floor.
4. Verify the new floor matches the upgrade's peer protocol version in the
   Zebra release notes, then update the pinned `Version` in each failing test.
   The manual pin is the acknowledgment step; do not compute the expected
   value from the dependency.
5. Fix any compile errors from Zebra API changes, and update docs whose
   statements the release invalidates.
6. Release Zeeder following the [release process](development.md#release-process)
   and publish the container image.

Ship promptly. Until the new Zeeder is deployed, seeders keep choosing peers
by the old floor and may serve nodes that will not follow the upgraded chain.

## Operator Procedure

Replacing the binary or image is the entire upgrade:

- Docker: pull the release image, then `docker compose up -d` to recreate the
  container. Keep the cache volume.
- systemd: build and install the release binary, then restart the unit
  ([upgrade steps](operations.md#upgrade)).

No configuration changes are needed. Zones, nameservers, TTLs, DNS delegation,
listener ports, rate limits, and cache paths are independent of network
upgrades.

Keep the peer cache. A cached peer is served only after a fresh handshake, and
the handshake applies the new floor, so outdated entries filter themselves
out.

### Verify After Deploying

```bash
curl -s http://127.0.0.1:9999/metrics | grep -E 'zeeder_min_protocol_version|zeeder_peers_servable'
```

- `zeeder_min_protocol_version` must report the new floor for the upgraded
  network, matching the Zebra release notes.
- `zeeder_peers_servable` for that network drops after the deploy. Only peers
  already running an upgrade-aware node release pass the new floor, so the
  count recovers at the pace the network's nodes upgrade. Shortly after an
  early testnet release, the dip can be deep and last for days.
- The `SeederLowPeerCount` alert can fire during that window. It measures
  network-wide upgrade adoption, not a seeder fault.
- `/ready` returns `503` for a zone below `health.ready_threshold`. The zone
  still answers SOA, NS, and NODATA, but its A and AAAA answers are empty
  until enough upgraded peers appear.

### If The Servable Set Stays Empty

Rolling the deployment back to the previous release restores the previous
floor, because the floor is compiled in. Weigh that against the purpose of the
floor: an empty answer is often better for bootstrapping nodes than peers that
may leave the post-upgrade chain. Prefer waiting while upgraded peers appear,
and roll back only when a network's node ecosystem clearly has not started
upgrading.

## Quick Reference

| Question | Answer |
|----------|--------|
| Does an upgrade require Zeeder config changes? | No |
| Is replacing the Docker container enough? | Yes; pull the new image and recreate the container |
| Must the peer cache be cleared? | No; the handshake re-filters cached peers |
| When should the new Zeeder be deployed? | As soon as it is released, before the activation height |
| What enforces the floor? | zebra-network's handshake, from the compiled activation table |
