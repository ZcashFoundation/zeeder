# Development Guide

Guide for contributors and developers working on zebra-seeder.

## Getting Started

### Prerequisites

- Rust from `rust-toolchain.toml` (`1.95.0`, with `clippy` and `rustfmt`)
- Git
- Docker (optional, for testing)

### Clone and Build

```bash
git clone https://github.com/zcashfoundation/zebra-seeder
cd zebra-seeder
cargo build
```

### Running Locally

```bash
# Copy example environment file
cp .env.example .env

# Edit .env for testnet (uses port 1053 to avoid requiring root)
# ZEBRA_SEEDER__DNS__LISTEN_ADDR="0.0.0.0:1053"
# ZEBRA_SEEDER__CRAWLER__NETWORK="Testnet"
# ZEBRA_SEEDER__DNS__DOMAIN="testnet.seeder.example.com"

# Run
cargo run start

# Test in another terminal
dig @127.0.0.1 -p 1053 testnet.seeder.example.com A
```

## Project Structure

```
zebra-seeder/
├── src/
│   ├── main.rs           # Entry point
│   ├── commands.rs       # CLI command handling
│   ├── config.rs         # Configuration structures
│   ├── crawl.rs          # Crawl-side module registration
│   ├── crawl/            # Chain tip, servability, and address cache
│   ├── dns.rs            # DNS-side module registration
│   ├── dns/              # DNS request handling and rate limiting
│   ├── seeder.rs         # Process composition and shutdown handling
│   └── metrics.rs        # Prometheus metrics setup
├── docs/                 # Documentation
├── Dockerfile            # Container image
├── docker-compose.yml    # Docker orchestration
├── Cargo.toml            # Dependencies
└── build.rs              # Build script (vergen)
```

### Key Files

- **`main.rs`**: Entry point, error handling setup
- **`commands.rs`**: CLI parsing, config loading, and command dispatch
- **`config.rs`**: `SeederConfig` struct, configuration loading
- **`seeder.rs`**: Composition root for crawling, DNS serving, and shutdown
- **`crawl/`**: Chain tip, peer servability, and servable peer cache
- **`dns/`**: DNS request handling and rate limiting
- **`metrics.rs`**: Prometheus metrics initialization

## Code Overview

### Main Components

**Configuration (`config.rs`):**
- `SeederConfig`: Main config struct
- `RateLimitConfig`: Rate limiting settings
- `MetricsConfig`: Metrics endpoint settings
- Serde deserialization from env vars/TOML

**Seeder (`seeder.rs`):**
- `run()`: Main seeder initialization and shutdown select

**Crawl (`crawl/`):**
- `SeederChainTip`: Protocol-version floor for zebra-network handshakes
- `classify_peer()`: Peer servability predicate
- `address_cache::spawn()`: Servable peer refresh loop and crawler monitoring
- `ServablePeers`: Shuffled, capped peer snapshot for DNS queries

**DNS (`dns/`):**
- `DnsRequestHandler`: DNS request handler (implements `RequestHandler`)
- `RateLimiter`: Per-IP rate limiting

**Commands (`commands.rs`):**
- CLI structure with clap
- Config loading orchestration
- Metrics initialization and command dispatch

## Testing

### Prerequisites

Install testing tools:

```bash
# Install cargo-nextest (faster test runner)
cargo install cargo-nextest --locked

# Install cargo-tarpaulin (coverage - Linux/CI only)
cargo install cargo-tarpaulin
```

### Run All Tests

```bash
# Using nextest (recommended)
cargo nextest run

# Or with standard cargo test
cargo test
```

### Run Specific Test

```bash
cargo nextest run test_rate_limit_default

# Or with cargo test
cargo test test_rate_limit_default
```

### Test Coverage

**Note**: `cargo-tarpaulin` only works on Linux. On macOS, run coverage in CI or use Docker.

```bash
# Generate coverage report (Linux/CI only)
cargo tarpaulin --ignore-tests --out Stdout

# Generate HTML report
cargo tarpaulin --ignore-tests --out Html --output-dir coverage

# View HTML report
open coverage/index.html
```

**Coverage in CI**: The GitHub Actions workflow automatically runs coverage on `ubuntu-latest` and uploads to Codecov.

### Test Structure

- **Unit tests**: Inline in source files (e.g., `src/crawl/servability.rs`)
- **Config tests**: `src/config.rs` (env var handling, defaults)
- **CLI tests**: `src/commands.rs` (argument parsing)
- **DNS handler tests**: `src/dns/request_handler.rs` (inline async tests)

### Adding Tests

Add new tests to the appropriate module:

**Unit test example:**
```rust
#[test]
fn test_new_feature() {
    // Your test here
}
```

**Async handler test example:**
```rust
#[tokio::test]
async fn test_dns_feature() {
    // Your async test here
}
```

For config tests that need environment variables, use `temp_env` so each test scopes its own variables:

```rust
#[test]
fn test_my_config() -> color_eyre::Result<()> {
    temp_env::with_var("ZEBRA_SEEDER__MY_PARAM", Some("value"), || {
        let config = SeederConfig::load_with_env(None)?;
        assert_eq!(config.my_param, "value");
        Ok(())
    })
}
```

## Maintenance

### Dependency Management

To keep dependencies up to date, use the following tools:

#### Check for Updates (Allowed by Semver)

To see what can be updated within your current `Cargo.toml` constraints (updating the `Cargo.lock` file):

```bash
cargo update --dry-run
```

To apply those updates:

```bash
cargo update
```

#### Check for Major/Minor Updates (Beyond Semver)

To see if new major or minor versions are available that require manual `Cargo.toml` changes:

```bash
# Install cargo-outdated first
cargo install cargo-outdated

# Run the check
cargo outdated -R -d 1
```

- `-R`: Root dependencies only (excludes transitives)
- `-d 1`: Depth 1 (ignores deep dependency trees)

## Code Style

### Formatting

```bash
# Auto-format code
cargo fmt --all

# Check formatting
cargo fmt --all -- --check
```

### Linting

```bash
# Run clippy
cargo clippy --all-targets --all-features -- -D warnings
```

### Pre-commit Checks

```bash
# Run all local validation gates
./commit_checks.sh
```

## Contributing

### Workflow

1. **Fork** the repository
2. **Create branch**: `git checkout -b feature/my-feature`
3. **Make changes** and add tests
4. **Run checks**: `./commit_checks.sh`
5. **Commit**: `git commit -m "feat: add new feature"`
6. **Push**: `git push origin feature/my-feature`
7. **Open PR** with description

### Commit Message Format

Follow conventional commits:

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

**Types:**
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation only
- `style`: Formatting, no code change
- `refactor`: Code change that neither fixes bug nor adds feature
- `test`: Adding tests
- `chore`: Maintenance

**Examples:**
```
feat(rate-limit): add configurable burst size
fix(dns): handle empty peer list correctly
docs: update deployment guide
```

## Common Development Tasks

### Adding New Configuration Parameter

1. Add the field to the config struct that owns the setting in `src/config.rs`
   (`DnsConfig`, `CrawlerConfig`, `MetricsConfig`, or `RateLimitConfig`):
```rust
pub(crate) struct DnsConfig {
    // ...
    pub(crate) my_param: String,
}
```

2. Update `Default` impl:
```rust
impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            // ...
            my_param: "default_value".to_string(),
        }
    }
}
```

3. Add validation in the owning config type when invalid values are possible, and
   call it from `SeederConfig::validate()`.

4. Add tests in the module that owns the config code (`src/config.rs`):
```rust
#[test]
fn test_my_param_default() {
    let config = DnsConfig::default();
    assert_eq!(config.my_param, "default_value");
}
```

5. Document the setting in `docs/operations.md` configuration table.

### Adding New Metric

1. Add the metric name or label to `src/metrics.rs`.

2. Emit the metric from the module that owns the event:
```rust
use metrics::counter;
use crate::metrics::MY_METRIC_TOTAL;

counter!(MY_METRIC_TOTAL).increment(1);
```

3. Document in `docs/operations.md` metrics table.

### Debugging

**Enable debug logging:**
```bash
RUST_LOG=debug cargo run start
```

**Trace-level (very verbose):**
```bash
RUST_LOG=zebra_seeder=trace cargo run start
```

**Filter by module:**
```bash
RUST_LOG=zebra_seeder::dns::request_handler=debug cargo run start
```

## Release Process

1. **Update version** in `Cargo.toml`
2. **Update CHANGELOG.md** with changes
3. **Run checks**: `./commit_checks.sh`
4. **Commit**: `git commit -m "chore: release v1.2.3"`
5. **Tag**: `git tag -a v1.2.3 -m "Release v1.2.3"`
6. **Push**: `git push && git push --tags`
7. **Build release**: `cargo build --release`
8. **Create GitHub release** with binaries

## Useful Resources

- [Zebra Project](https://github.com/ZcashFoundation/zebra) - Zcash full node
- [hickory-server Docs](https://docs.rs/hickory-server/) - DNS server library
- [governor Docs](https://docs.rs/governor/) - Rate limiting
- [Zcash Protocol Spec](https://zips.z.cash/protocol/protocol.pdf)

## Getting Help

- Open an issue on GitHub
- Join Zcash community channels
- Read through existing code and tests
