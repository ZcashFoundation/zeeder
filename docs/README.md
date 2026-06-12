# Zeeder Documentation

Documentation for the Zcash DNS seeder.

## Quick Navigation

- **[Architecture](architecture.md)** - System design, components, and key decisions (ADRs)
- **[Operations](operations.md)** - Production topology, DNS delegation, deployment, monitoring, and troubleshooting
- **[Development](development.md)** - Contributing and development guide
- **[Context](../CONTEXT.md)** - Glossary for load-bearing project terms

## Start Here

Operators should start with [Operations](operations.md). It explains how many
Zeeder services to run, how to delegate DNS, how to deploy Docker or systemd
services, and how to verify each authoritative nameserver.

Developers should read [Architecture](architecture.md), then
[Development](development.md). Code reviewers should start with
[Architecture](architecture.md).

## What Is Zeeder?

A DNS seeder for Zcash that:
- Crawls the network to discover healthy peers
- Serves DNS A/AAAA records to clients
- Implements rate limiting to prevent DDoS abuse
- Provides Prometheus metrics

Built with Rust using `zebra-network` and Hickory DNS.
