# Zeeder Documentation

Documentation for the Zcash DNS seeder.

## Quick Navigation

- **[Architecture](architecture.md)** - System design, components, and key decisions (ADRs)
- **[Operations](operations.md)** - Production topology, DNS delegation, deployment, monitoring, and troubleshooting
- **[Network Upgrades](network-upgrades.md)** - Runbook for each Zebra release that activates a network upgrade
- **[Development](development.md)** - Contributing and development guide
- **[Context](../CONTEXT.md)** - Glossary for load-bearing project terms

## Start Here

Operators should start with [Operations](operations.md). It explains how one
process serves every network, how to delegate each zone's DNS, how to deploy
Docker or systemd, and how to verify each authoritative nameserver.

Developers should read [Architecture](architecture.md), then
[Development](development.md). Code reviewers should start with
[Architecture](architecture.md).

## What Is Zeeder?

A DNS seeder for Zcash that:
- Crawls every configured network (mainnet, testnet) with an independent crawler each
- Serves a DNS zone per network on one shared listener
- Implements rate limiting to prevent DDoS abuse
- Provides Prometheus metrics and a health endpoint

Built with Rust using `zebra-network` and Hickory DNS.
