# zeeder

[![Rust CI](https://github.com/zcashfoundation/zeeder/actions/workflows/ci.yml/badge.svg)](https://github.com/zcashfoundation/zeeder/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/zcashfoundation/zeeder/branch/main/graph/badge.svg)](https://codecov.io/gh/zcashfoundation/zeeder)

A Rust-based DNS seeder for the Zcash network, mirroring patterns from the [Zebra](https://github.com/zcashfoundation/zebra) project.

## Status

**Current State**: Pre-release. Internal refactor and validation in progress.

## Quickstart

```bash
cp .env.example .env
cargo run -- start
```

Verify DNS responses in another terminal:

```bash
dig @127.0.0.1 -p 1053 testnet.seeder.example.com A
dig @127.0.0.1 -p 1053 testnet.seeder.example.com AAAA
dig @127.0.0.1 -p 1053 testnet.seeder.example.com SOA
```

## Documentation

- [Architecture](docs/architecture.md): system design, component boundaries, data flow, and ADR index
- [Operations](docs/operations.md): production topology, DNS delegation, deployment, monitoring, and troubleshooting
- [Development](docs/development.md): local setup, project structure, testing, and maintenance
- [Context](CONTEXT.md): glossary for load-bearing project terms

## License

This project is licensed under either of:

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
