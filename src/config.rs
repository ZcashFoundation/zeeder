//! Layered configuration for the seeder: defaults, then an optional TOML file,
//! then `ZEEDER__*` environment variables (each layer overriding the last).
//!
//! One process serves one DNS zone per Zcash network. Each `[zones.<network>]`
//! entry binds a network to its authoritative DNS identity; all zones share a
//! single DNS listener, rate limiter, and metrics endpoint.

use color_eyre::eyre::{Context, Result, ensure, eyre};
use config::{Config, Environment, File};
use hickory_proto::rr::{LowerName, Name};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroU32,
};
use zebra_chain::parameters::Network;

/// Zeeder process configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SeederConfig {
    /// Shared DNS server settings (one listener serves every zone).
    pub(crate) dns: DnsConfig,

    /// Per-network served zones, keyed by Zcash network (`mainnet`, `testnet`).
    pub(crate) zones: BTreeMap<ZcashNetwork, ZoneConfig>,

    /// Prometheus metrics configuration. If `None`, metrics are disabled.
    pub(crate) metrics: Option<MetricsConfig>,

    /// Health and readiness endpoint. If `None`, the endpoint is disabled.
    pub(crate) health: Option<HealthConfig>,

    /// Rate limiting configuration. If `None`, rate limiting is disabled (not
    /// recommended for production).
    pub(crate) rate_limit: Option<RateLimitConfig>,
}

/// Zcash network choices the seeder can crawl and serve.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ZcashNetwork {
    /// The production Zcash network.
    Mainnet,

    /// The default public Zcash test network.
    Testnet,
}

impl ZcashNetwork {
    /// Convert to the zebra-chain network this maps onto.
    pub(crate) fn to_zebra(self) -> Network {
        match self {
            Self::Mainnet => Network::Mainnet,
            Self::Testnet => Network::new_default_testnet(),
        }
    }

    /// Stable lowercase label, used as the `network` metric label value and in
    /// log fields.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
        }
    }

    /// Build the zebra-network configuration for this network's crawler.
    ///
    /// The P2P listener binds the network's default port (mainnet `8233`,
    /// testnet `18233`), so two crawlers in one process never collide. The
    /// peer cache file is network-keyed by zebra-network itself.
    pub(crate) fn network_config(self) -> zebra_network::Config {
        let network = self.to_zebra();
        let listen_addr =
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), network.default_port());

        zebra_network::Config {
            network,
            listen_addr,
            external_addr: None,
            max_connections_per_ip: 1,
            ..zebra_network::Config::default()
        }
    }
}

/// Authoritative DNS identity for a single network's seed zone.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ZoneConfig {
    /// The domain name this zone is authoritative for.
    pub(crate) domain: String,

    /// The authoritative nameserver for `domain`.
    ///
    /// This must be outside `domain` because Zeeder does not serve address
    /// records for nameserver hostnames.
    pub(crate) nameserver: String,

    /// DNS response TTL (time to live) in seconds for this zone.
    ///
    /// Lower values mean fresher data but more queries. Testnet typically uses
    /// a shorter TTL than mainnet because its peer set churns faster.
    pub(crate) ttl: u32,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        Self {
            domain: String::new(),
            nameserver: String::new(),
            ttl: 600,
        }
    }
}

impl ZoneConfig {
    /// Parse and validate this zone, returning its domain for cross-zone checks.
    fn validated_domain(&self, network: ZcashNetwork) -> Result<LowerName> {
        let network = network.label();
        let domain = parse_dns_name(&format!("zones.{network}.domain"), &self.domain)?;
        let nameserver = parse_dns_name(&format!("zones.{network}.nameserver"), &self.nameserver)?;
        ensure_nameserver_is_out_of_zone(&domain, &nameserver)?;

        Ok(LowerName::from(domain))
    }
}

/// Shared DNS server configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DnsConfig {
    /// The socket address Hickory DNS binds to, serving every configured zone.
    ///
    /// Defaults to `0.0.0.0:53`.
    pub(crate) listen_addr: SocketAddr,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 53),
        }
    }
}

fn parse_dns_name(field: &str, name_text: &str) -> Result<Name> {
    let mut name = Name::from_ascii(name_text)
        .wrap_err_with(|| format!("{field} must be a valid DNS name: {name_text:?}"))?;
    name.set_fqdn(true);
    Ok(name)
}

fn ensure_nameserver_is_out_of_zone(domain: &Name, nameserver: &Name) -> Result<()> {
    let domain = LowerName::from(domain.clone());
    let nameserver = LowerName::from(nameserver.clone());

    ensure!(
        !domain.zone_of(&nameserver),
        "nameserver must be outside its zone domain because Zeeder does not serve address records for nameserver hostnames"
    );

    Ok(())
}

/// Configuration for Prometheus metrics.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct MetricsConfig {
    /// The socket address to expose Prometheus metrics on. Defaults to
    /// `0.0.0.0:9999`.
    pub(crate) endpoint_addr: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            endpoint_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9999),
        }
    }
}

/// Configuration for the health and readiness endpoint.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct HealthConfig {
    /// The socket address to serve `/health` and `/ready` on. Defaults to
    /// `0.0.0.0:8080`.
    pub(crate) endpoint_addr: SocketAddr,

    /// Minimum servable peers a zone needs before it reports ready. Defaults to
    /// `1`. A zone with fewer servable peers makes `/ready` return `503`.
    pub(crate) ready_threshold: usize,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            endpoint_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080),
            ready_threshold: 1,
        }
    }
}

/// Configuration for DNS query rate limiting.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RateLimitConfig {
    /// Maximum queries per second per IP address. Defaults to `10`.
    pub(crate) queries_per_second: u32,

    /// Burst capacity (maximum queries in a short burst). Defaults to `20`
    /// (twice the rate).
    pub(crate) burst_size: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            queries_per_second: 10,
            burst_size: 20,
        }
    }
}

impl RateLimitConfig {
    pub(crate) fn nonzero_queries_per_second(&self) -> Result<NonZeroU32> {
        nonzero_config_value("rate_limit.queries_per_second", self.queries_per_second)
    }

    pub(crate) fn nonzero_burst_size(&self) -> Result<NonZeroU32> {
        nonzero_config_value("rate_limit.burst_size", self.burst_size)
    }

    fn validate(&self) -> Result<()> {
        self.nonzero_queries_per_second()?;
        self.nonzero_burst_size()?;
        Ok(())
    }
}

fn nonzero_config_value(field: &str, configured_value: u32) -> Result<NonZeroU32> {
    NonZeroU32::new(configured_value).ok_or_else(|| eyre!("{field} must be greater than 0"))
}

impl Default for SeederConfig {
    fn default() -> Self {
        Self {
            dns: DnsConfig::default(),
            zones: BTreeMap::new(),
            metrics: None,
            health: None,
            rate_limit: Some(RateLimitConfig::default()),
        }
    }
}

impl SeederConfig {
    /// Load configuration, layering defaults, an optional TOML file, and
    /// environment variables.
    ///
    /// Precedence, lowest to highest:
    /// 1. Default values
    /// 2. Config file (if a path is provided)
    /// 3. Environment variables (`ZEEDER__*`)
    pub(crate) fn load_with_env(path: Option<std::path::PathBuf>) -> Result<Self> {
        let mut builder = Config::builder().add_source(Config::try_from(&Self::default())?);

        if let Some(path) = path {
            builder = builder.add_source(File::from(path));
        }

        builder = builder.add_source(
            Environment::with_prefix("ZEEDER")
                .separator("__")
                .try_parsing(true),
        );

        let seeder_config: Self = builder.build()?.try_deserialize()?;
        seeder_config.validate()?;

        Ok(seeder_config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.zones.is_empty(),
            "at least one zone must be configured, e.g. [zones.mainnet] or [zones.testnet]"
        );

        let mut zone_domains: Vec<(ZcashNetwork, LowerName)> = Vec::new();
        for (network, zone) in &self.zones {
            zone_domains.push((*network, zone.validated_domain(*network)?));
        }
        ensure_zones_are_disjoint(&zone_domains)?;

        if let Some(rate_limit) = &self.rate_limit {
            rate_limit.validate()?;
        }

        Ok(())
    }
}

/// Reject zones whose domains are equal or nested, so a query routes to exactly
/// one zone.
fn ensure_zones_are_disjoint(zone_domains: &[(ZcashNetwork, LowerName)]) -> Result<()> {
    for (outer_index, (left_network, left_domain)) in zone_domains.iter().enumerate() {
        for (right_network, right_domain) in &zone_domains[outer_index + 1..] {
            ensure!(
                !left_domain.zone_of(right_domain) && !right_domain.zone_of(left_domain),
                "zones {} ({left_domain}) and {} ({right_domain}) overlap; each zone domain must be disjoint so queries route unambiguously",
                left_network.label(),
                right_network.label(),
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    type TestResult<T = ()> = color_eyre::Result<T>;

    fn config_file_path(name: &str) -> TestResult<PathBuf> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();

        Ok(std::env::temp_dir().join(format!("zeeder-{name}-{timestamp}.toml")))
    }

    /// A minimal valid single-zone TOML body for tests that need a config file.
    const MAINNET_ZONE_TOML: &str = r#"
[zones.mainnet]
domain = "mainnet.seeder.example.com"
nameserver = "ns-mainnet.seeder.example.com"
ttl = 600
"#;

    #[test]
    fn test_default_config() {
        let config = SeederConfig::default();
        assert_eq!(config.dns.listen_addr.to_string(), "0.0.0.0:53");
        assert!(
            config.zones.is_empty(),
            "the default ships no zones; operators declare their own"
        );
    }

    #[test]
    fn default_config_fails_validation_without_zones() {
        let config = temp_env::with_vars(no_env_overrides(), || SeederConfig::load_with_env(None));

        assert!(
            config.is_err(),
            "a config with no zones should fail validation"
        );
    }

    #[test]
    fn test_network_config_ports_match_their_network() {
        assert_eq!(
            ZcashNetwork::Mainnet.network_config().listen_addr.port(),
            Network::Mainnet.default_port()
        );
        assert_eq!(
            ZcashNetwork::Testnet.network_config().listen_addr.port(),
            Network::new_default_testnet().default_port()
        );
        assert!(
            ZcashNetwork::Mainnet
                .network_config()
                .external_addr
                .is_none()
        );
        assert_eq!(
            ZcashNetwork::Mainnet
                .network_config()
                .max_connections_per_ip,
            1
        );
    }

    #[test]
    fn operations_docs_include_crawler_listener_firewall_ports() {
        let operations_docs = include_str!("../docs/operations.md");

        for port in [
            Network::Mainnet.default_port(),
            Network::new_default_testnet().default_port(),
        ] {
            let firewall_command = format!("ufw allow {port}/tcp");
            assert!(
                operations_docs.contains(&firewall_command),
                "`{firewall_command}` should be documented in the firewall checklist"
            );
        }
    }

    #[test]
    fn operations_docs_include_config_reference_rows() {
        let operations_docs = include_str!("../docs/operations.md");
        let expected_rows = [
            "| `dns.listen_addr` | `ZEEDER__DNS__LISTEN_ADDR` | `0.0.0.0:53` | Shared DNS listener for every zone |",
            "| `zones.<network>.domain` | `ZEEDER__ZONES__<NETWORK>__DOMAIN` | (none) | Authoritative domain for that network |",
            "| `zones.<network>.nameserver` | `ZEEDER__ZONES__<NETWORK>__NAMESERVER` | (none) | Out-of-zone authoritative nameserver |",
            "| `zones.<network>.ttl` | `ZEEDER__ZONES__<NETWORK>__TTL` | `600` | DNS response TTL in seconds |",
            "| `rate_limit.queries_per_second` | `ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND` | `10` | Max queries/sec per IP; must be greater than 0 |",
            "| `rate_limit.burst_size` | `ZEEDER__RATE_LIMIT__BURST_SIZE` | `20` | Burst capacity; must be greater than 0 |",
            "| `metrics.endpoint_addr` | `ZEEDER__METRICS__ENDPOINT_ADDR` | (disabled) | Prometheus endpoint |",
            "| `health.endpoint_addr` | `ZEEDER__HEALTH__ENDPOINT_ADDR` | (disabled) | Health and readiness endpoint |",
            "| `health.ready_threshold` | `ZEEDER__HEALTH__READY_THRESHOLD` | `1` | Servable peers per zone required for readiness |",
        ];

        for row in expected_rows {
            assert!(
                operations_docs.contains(row),
                "docs/operations.md should document `{row}`"
            );
        }
    }

    /// Env keys present in `.env.example`, paired with `None` so each is cleared
    /// from the ambient environment before the example is layered back on.
    fn env_example_overrides() -> Vec<(&'static str, Option<&'static str>)> {
        [
            "ZEEDER__DNS__LISTEN_ADDR",
            "ZEEDER__ZONES__MAINNET__DOMAIN",
            "ZEEDER__ZONES__MAINNET__NAMESERVER",
            "ZEEDER__ZONES__MAINNET__TTL",
            "ZEEDER__ZONES__TESTNET__DOMAIN",
            "ZEEDER__ZONES__TESTNET__NAMESERVER",
            "ZEEDER__ZONES__TESTNET__TTL",
            "ZEEDER__METRICS__ENDPOINT_ADDR",
            "ZEEDER__HEALTH__ENDPOINT_ADDR",
            "ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND",
            "ZEEDER__RATE_LIMIT__BURST_SIZE",
        ]
        .into_iter()
        .map(|key| (key, None))
        .collect()
    }

    /// Clears every `ZEEDER__*` key this test module sets, so ambient
    /// environment never leaks into a `load_with_env` call.
    fn no_env_overrides() -> Vec<(&'static str, Option<&'static str>)> {
        env_example_overrides()
    }

    #[test]
    fn env_example_loads_with_supported_config_keys() -> TestResult {
        let env_example_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env.example");
        let mut env_example_vars = Vec::new();

        for item in dotenvy::from_path_iter(&env_example_path)? {
            let (key, value) = item?;
            env_example_vars.push((key, Some(value)));
        }

        let actual_keys: Vec<&str> = env_example_vars
            .iter()
            .map(|(key, _value)| key.as_str())
            .collect();
        let expected_keys: Vec<&str> = env_example_overrides()
            .iter()
            .map(|(key, _value)| *key)
            .collect();
        assert_eq!(actual_keys.as_slice(), expected_keys.as_slice());

        let scoped_env: Vec<(&str, Option<&str>)> = env_example_vars
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_deref()))
            .collect();
        let config = temp_env::with_vars(&scoped_env, || SeederConfig::load_with_env(None))?;

        let mainnet = config
            .zones
            .get(&ZcashNetwork::Mainnet)
            .ok_or_else(|| eyre!("mainnet zone should load"))?;
        let testnet = config
            .zones
            .get(&ZcashNetwork::Testnet)
            .ok_or_else(|| eyre!("testnet zone should load"))?;
        assert_eq!(config.dns.listen_addr.to_string(), "0.0.0.0:1053");
        assert_eq!(mainnet.domain, "mainnet.seeder.example.com");
        assert_eq!(testnet.domain, "testnet.seeder.example.com");
        assert_eq!(testnet.ttl, 300);

        Ok(())
    }

    #[test]
    fn test_rate_limit_default() {
        let config = SeederConfig::default();
        assert_eq!(
            config
                .rate_limit
                .map(|rl| (rl.queries_per_second, rl.burst_size)),
            Some((10, 20))
        );
    }

    #[test]
    fn config_with_zones_serializes_to_toml() -> TestResult {
        // `print-config` renders the resolved config as TOML, so a config that
        // includes zones (network-keyed map) must be TOML-serializable.
        let config = temp_env::with_vars(no_env_overrides(), || {
            let path = config_file_path("toml-roundtrip")?;
            fs::write(&path, MAINNET_ZONE_TOML)?;
            let config = SeederConfig::load_with_env(Some(path.clone()));
            fs::remove_file(&path)?;
            config
        })?;

        let rendered = toml::to_string_pretty(&config)?;
        assert!(
            rendered.contains("[zones.mainnet]"),
            "serialized config should render the network-keyed zone table"
        );
        Ok(())
    }

    #[test]
    fn loads_multi_zone_toml_config_file() -> TestResult {
        let path = config_file_path("config")?;
        fs::write(
            &path,
            r#"
[dns]
listen_addr = "127.0.0.1:1053"

[zones.mainnet]
domain = "mainnet.seeder.example.com"
nameserver = "ns-mainnet.seeder.example.com"
ttl = 600

[zones.testnet]
domain = "testnet.seeder.example.com"
nameserver = "ns-testnet.seeder.example.com"
ttl = 300

[rate_limit]
queries_per_second = 50
burst_size = 100

[metrics]
endpoint_addr = "127.0.0.1:9999"
"#,
        )?;

        let config = temp_env::with_vars(no_env_overrides(), || {
            SeederConfig::load_with_env(Some(path.clone()))
        });
        fs::remove_file(&path)?;
        let config = config?;

        assert_eq!(config.dns.listen_addr.to_string(), "127.0.0.1:1053");
        assert_eq!(config.zones.len(), 2);
        let mainnet = config
            .zones
            .get(&ZcashNetwork::Mainnet)
            .ok_or_else(|| eyre!("mainnet zone should load"))?;
        assert_eq!(mainnet.domain, "mainnet.seeder.example.com");
        assert_eq!(mainnet.ttl, 600);
        assert_eq!(
            config.zones[&ZcashNetwork::Testnet].ttl,
            300,
            "each zone keeps its own ttl"
        );
        assert_eq!(
            config
                .rate_limit
                .map(|rate_limit| (rate_limit.queries_per_second, rate_limit.burst_size)),
            Some((50, 100))
        );
        Ok(())
    }

    #[test]
    fn env_populates_and_overrides_zones() -> TestResult {
        let path = config_file_path("env-zone")?;
        fs::write(&path, MAINNET_ZONE_TOML)?;

        let config = temp_env::with_vars(
            [
                (
                    "ZEEDER__ZONES__MAINNET__DOMAIN",
                    Some("override.seeder.example.com"),
                ),
                (
                    "ZEEDER__ZONES__TESTNET__DOMAIN",
                    Some("testnet.seeder.example.com"),
                ),
                (
                    "ZEEDER__ZONES__TESTNET__NAMESERVER",
                    Some("ns-testnet.seeder.example.com"),
                ),
                ("ZEEDER__ZONES__TESTNET__TTL", Some("300")),
            ],
            || SeederConfig::load_with_env(Some(path.clone())),
        );
        fs::remove_file(&path)?;
        let config = config?;

        assert_eq!(
            config.zones[&ZcashNetwork::Mainnet].domain,
            "override.seeder.example.com",
            "env overrides a file-defined zone field"
        );
        assert_eq!(
            config.zones[&ZcashNetwork::Testnet].domain,
            "testnet.seeder.example.com",
            "env adds a zone not present in the file"
        );
        Ok(())
    }

    #[test]
    fn unknown_zone_field_is_rejected() {
        let config = temp_env::with_vars(
            [
                (
                    "ZEEDER__ZONES__MAINNET__DOMAIN",
                    Some("mainnet.seeder.example.com"),
                ),
                (
                    "ZEEDER__ZONES__MAINNET__NAMESERVER",
                    Some("ns-mainnet.seeder.example.com"),
                ),
                ("ZEEDER__ZONES__MAINNET__TTTL", Some("600")),
            ],
            || SeederConfig::load_with_env(None),
        );

        assert!(config.is_err(), "an unknown zone field should fail");
    }

    #[test]
    fn flat_dns_env_key_is_rejected() {
        let config = temp_env::with_vars(single_mainnet_env("ZEEDER__DNS_TTL", "300"), || {
            SeederConfig::load_with_env(None)
        });

        assert!(config.is_err(), "flat DNS config key should fail");
    }

    #[test]
    fn upstream_network_env_key_is_rejected() {
        let config = temp_env::with_vars(
            single_mainnet_env("ZEEDER__NETWORK__NETWORK", "Testnet"),
            || SeederConfig::load_with_env(None),
        );

        assert!(
            config.is_err(),
            "upstream zebra-network config key should fail"
        );
    }

    #[test]
    fn zebra_namespace_env_key_is_ignored() -> TestResult {
        let config = temp_env::with_vars(
            single_mainnet_env("ZEBRA_NETWORK__NETWORK", "Testnet"),
            || SeederConfig::load_with_env(None),
        )?;

        assert_eq!(config.zones.len(), 1, "only the mainnet zone should load");
        assert!(config.zones.contains_key(&ZcashNetwork::Mainnet));
        Ok(())
    }

    /// A valid single mainnet zone declared entirely via env, plus one extra
    /// key under test.
    fn single_mainnet_env(
        extra_key: &'static str,
        extra_value: &'static str,
    ) -> Vec<(&'static str, Option<&'static str>)> {
        vec![
            (
                "ZEEDER__ZONES__MAINNET__DOMAIN",
                Some("mainnet.seeder.example.com"),
            ),
            (
                "ZEEDER__ZONES__MAINNET__NAMESERVER",
                Some("ns-mainnet.seeder.example.com"),
            ),
            (extra_key, Some(extra_value)),
        ]
    }

    #[test]
    fn invalid_zone_domain_is_rejected() {
        let config = temp_env::with_vars(
            [
                ("ZEEDER__ZONES__MAINNET__DOMAIN", Some("not a domain")),
                (
                    "ZEEDER__ZONES__MAINNET__NAMESERVER",
                    Some("ns-mainnet.seeder.example.com"),
                ),
            ],
            || SeederConfig::load_with_env(None),
        );

        assert!(config.is_err(), "invalid zone domain should fail");
    }

    #[test]
    fn in_zone_nameserver_is_rejected() {
        let config = temp_env::with_vars(
            [
                (
                    "ZEEDER__ZONES__TESTNET__DOMAIN",
                    Some("testnet.seeder.example.com"),
                ),
                (
                    "ZEEDER__ZONES__TESTNET__NAMESERVER",
                    Some("ns.testnet.seeder.example.com"),
                ),
            ],
            || SeederConfig::load_with_env(None),
        );

        assert!(
            config.is_err(),
            "in-zone nameserver should fail because Zeeder does not serve glue"
        );
    }

    #[test]
    fn overlapping_zone_domains_are_rejected() {
        let config = temp_env::with_vars(
            [
                ("ZEEDER__ZONES__MAINNET__DOMAIN", Some("seeder.example.com")),
                ("ZEEDER__ZONES__MAINNET__NAMESERVER", Some("ns.example.com")),
                (
                    "ZEEDER__ZONES__TESTNET__DOMAIN",
                    Some("testnet.seeder.example.com"),
                ),
                (
                    "ZEEDER__ZONES__TESTNET__NAMESERVER",
                    Some("ns-testnet.example.com"),
                ),
            ],
            || SeederConfig::load_with_env(None),
        );

        assert!(
            config.is_err(),
            "a zone nested inside another should fail to keep routing unambiguous"
        );
    }

    #[test]
    fn unknown_metrics_env_key_is_rejected() {
        let config = temp_env::with_vars(
            single_mainnet_env("ZEEDER__METRICS__ENDPOINT_ADRR", "127.0.0.1:0"),
            || SeederConfig::load_with_env(None),
        );

        assert!(config.is_err(), "unknown metrics config key should fail");
    }

    #[test]
    fn metrics_endpoint_env_enables_metrics() -> TestResult {
        let config = temp_env::with_vars(
            single_mainnet_env("ZEEDER__METRICS__ENDPOINT_ADDR", "127.0.0.1:9999"),
            || SeederConfig::load_with_env(None),
        )?;

        assert_eq!(
            config
                .metrics
                .map(|metrics| metrics.endpoint_addr.to_string()),
            Some("127.0.0.1:9999".to_string())
        );
        Ok(())
    }

    #[test]
    fn health_endpoint_env_enables_health() -> TestResult {
        let config = temp_env::with_vars(
            single_mainnet_env("ZEEDER__HEALTH__ENDPOINT_ADDR", "127.0.0.1:8080"),
            || SeederConfig::load_with_env(None),
        )?;

        let health = config
            .health
            .ok_or_else(|| eyre!("health endpoint should be enabled"))?;
        assert_eq!(health.endpoint_addr.to_string(), "127.0.0.1:8080");
        assert_eq!(health.ready_threshold, 1, "default threshold applies");
        Ok(())
    }

    #[test]
    fn zero_rate_limit_is_rejected() {
        let config = temp_env::with_vars(
            single_mainnet_env("ZEEDER__RATE_LIMIT__QUERIES_PER_SECOND", "0"),
            || SeederConfig::load_with_env(None),
        );

        assert!(config.is_err(), "zero query rate should fail");
    }

    #[test]
    fn per_zone_ttl_from_env() -> TestResult {
        let config = temp_env::with_vars(
            [
                (
                    "ZEEDER__ZONES__TESTNET__DOMAIN",
                    Some("testnet.seeder.example.com"),
                ),
                (
                    "ZEEDER__ZONES__TESTNET__NAMESERVER",
                    Some("ns-testnet.seeder.example.com"),
                ),
                ("ZEEDER__ZONES__TESTNET__TTL", Some("300")),
            ],
            || SeederConfig::load_with_env(None),
        )?;
        assert_eq!(config.zones[&ZcashNetwork::Testnet].ttl, 300);
        Ok(())
    }
}
