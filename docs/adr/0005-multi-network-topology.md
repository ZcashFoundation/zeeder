# ADR 0005: Multi-Network Serving Topology

## Status

Accepted

## Context

A Zcash DNS seeder operator typically serves more than one network. The earlier
CoreDNS-based seeder ran one process with a server block per network, so a single
deployment answered `mainnet.seeder.example.com` and `testnet.seeder.example.com`
on one IP and port 53.

An interim Zeeder design served exactly one network per process: a single
`crawler.network` and a single `dns.domain`. Serving both networks then required
two processes on two public IPs, a deployment regression against the CoreDNS
seeder for operators migrating an existing seed domain.

## Decision

One Zeeder process serves every configured network. Configuration is a map keyed
by network (`[zones.mainnet]`, `[zones.testnet]`), where each entry binds a
network to its authoritative DNS identity (`domain`, `nameserver`, `ttl`). The
process runs one independent `zebra_network` crawler per network and answers all
of their zones on a single shared DNS listener.

A single `DnsRequestHandler` owns the set of zones and routes each query to the
zone whose domain contains the query name, returning REFUSED for names outside
every zone. The shared per-IP rate limiter runs before routing, so its silent
drop and amplification defense are unchanged. One `metrics` endpoint and one
health endpoint serve the whole process.

The supported network set is closed to mainnet and testnet. Per-network metrics
(`zeeder_peers_servable`, `zeeder_peers_unservable`, `zeeder_peers_known`,
`zeeder_min_protocol_version`) carry a `network` label.

## Rationale

- This restores deployment parity with the CoreDNS seeder: one process, one IP,
  every network, which is the migration path operators expect.
- A network-keyed map makes per-network uniqueness structural and keeps every
  field overridable by environment variable (`ZEEDER__ZONES__MAINNET__DOMAIN`),
  so the env-driven Docker workflow still configures multiple zones. The config
  crate cannot populate an array of tables from environment variables, so an
  array shape would have forced operators onto a TOML file.
- Extending the existing custom request handler to route a zone set is a thin
  generalization of the single-zone `zone_of` match already in place. Hickory's
  `Catalog` and `ZoneHandler` model a record store with AXFR, NXDOMAIN proofs,
  and DNSSEC, none of which a synthetic seeder needs, and offer no hook for the
  silent rate-limit drop, so adopting them would add surface without removing
  work.
- Running one crawler per network in one process is safe: each crawler keeps its
  state behind the values `zebra_network::init` returns, the peer cache files are
  network-keyed (`network/<name>.peers`), and the P2P listeners bind distinct
  default ports.

## Consequences

- A single deployment serves both networks on one public IP and port 53.
- The process holds roughly one crawler's worth of peer connections per network,
  so a two-network process needs about double the file descriptors and P2P
  bandwidth of a single-network process.
- One host now needs outbound P2P egress to both 8233 (mainnet) and 18233
  (testnet).
- Regtest cannot be served alongside testnet, because both use P2P port 18233. A
  third concurrent network is not supported.
- zebra-network's own internal metrics (`zcash.net.*`, `candidate_set.*`,
  `pool.*`) carry no network label, so with two crawlers in one process their
  values are combined across networks. Per-network observability comes from the
  `zeeder_*` metrics, which carry the `network` label. Splitting zebra's internal
  metrics per network would require a per-instance recorder layer and is out of
  scope.

## Alternatives Considered

- One process per network (the interim design): rejected because it breaks
  one-IP deployment parity with the CoreDNS seeder and forces two public IPs to
  serve two networks.
- Array-of-tables config (`[[zone]]`): rejected because the config crate's
  environment source cannot populate or override array entries, which would break
  the env-only deployment workflow.
- Hickory `Catalog` and `ZoneHandler`: rejected because their record-store data
  model does not fit a synthetic seeder and leaves no place for the silent
  rate-limit drop.
