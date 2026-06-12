//! Layered configuration for the seeder: defaults, then an optional TOML file,
//! then `ZEBRA_SEEDER__*` environment variables (each layer overriding the last).

use color_eyre::eyre::{Context, Result, eyre};
use config::{Config, Environment, File};
use hickory_proto::rr::Name;
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroU32,
};
use zebra_chain::parameters::Network;

/// Configuration for the Zebra seeder.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SeederConfig {
    /// Zebra-network crawler configuration.
    pub(crate) crawler: CrawlerConfig,

    /// DNS server configuration.
    pub(crate) dns: DnsConfig,

    /// Prometheus metrics configuration. If `None`, metrics are disabled.
    pub(crate) metrics: Option<MetricsConfig>,

    /// Rate limiting configuration. If `None`, rate limiting is disabled (not
    /// recommended for production).
    pub(crate) rate_limit: Option<RateLimitConfig>,
}

/// Zebra-network crawler configuration exposed by the seeder.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CrawlerConfig {
    /// The Zcash network to crawl.
    pub(crate) network: CrawlerNetwork,
}

impl CrawlerConfig {
    /// Build the underlying zebra-network configuration.
    pub(crate) fn network_config(&self) -> zebra_network::Config {
        let network = self.network.zcash_network();
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

/// Zcash network choices supported by the seeder crawler.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum CrawlerNetwork {
    /// The production Zcash network.
    #[default]
    Mainnet,

    /// The default public Zcash test network.
    Testnet,
}

impl CrawlerNetwork {
    fn zcash_network(self) -> Network {
        match self {
            Self::Mainnet => Network::Mainnet,
            Self::Testnet => Network::new_default_testnet(),
        }
    }
}

/// DNS server configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DnsConfig {
    /// The socket address Hickory DNS will bind to.
    ///
    /// Defaults to `0.0.0.0:53`.
    pub(crate) listen_addr: SocketAddr,

    /// The domain name the seeder is authoritative for.
    pub(crate) domain: String,

    /// DNS response TTL (time to live) in seconds.
    ///
    /// Controls how long clients cache DNS responses. Lower values mean fresher
    /// data but more queries; higher values reduce load but propagate updates
    /// more slowly. Defaults to `600` (10 minutes).
    pub(crate) ttl: u32,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 53),
            domain: "mainnet.seeder.example.com".to_string(),
            ttl: 600,
        }
    }
}

impl DnsConfig {
    fn validate(&self) -> Result<()> {
        Name::from_ascii(&self.domain)
            .wrap_err_with(|| format!("dns.domain must be a valid DNS name: {:?}", self.domain))?;

        Ok(())
    }
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
            crawler: CrawlerConfig::default(),
            dns: DnsConfig::default(),
            metrics: None,
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
    /// 3. Environment variables (`ZEBRA_SEEDER__*`)
    pub(crate) fn load_with_env(path: Option<std::path::PathBuf>) -> Result<Self> {
        let mut builder = Config::builder().add_source(Config::try_from(&Self::default())?);

        if let Some(path) = path {
            builder = builder.add_source(File::from(path));
        }

        builder = builder.add_source(
            Environment::with_prefix("ZEBRA_SEEDER")
                .separator("__")
                .try_parsing(true),
        );

        let seeder_config: Self = builder.build()?.try_deserialize()?;
        seeder_config.validate()?;

        Ok(seeder_config)
    }

    fn validate(&self) -> Result<()> {
        self.dns.validate()?;

        if let Some(rate_limit) = &self.rate_limit {
            rate_limit.validate()?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = color_eyre::Result<()>;

    #[test]
    fn test_default_config() {
        let config = SeederConfig::default();
        assert_eq!(config.dns.listen_addr.to_string(), "0.0.0.0:53");
        assert_eq!(config.dns.domain, "mainnet.seeder.example.com");
        assert_eq!(config.dns.ttl, 600);
    }

    #[test]
    fn test_network_config_defaults() {
        let config = SeederConfig::default();
        assert_eq!(config.crawler.network, CrawlerNetwork::Mainnet);
        assert_eq!(
            config.crawler.network_config().listen_addr.port(),
            Network::Mainnet.default_port()
        );
        assert!(config.crawler.network_config().external_addr.is_none());
        assert_eq!(config.crawler.network_config().max_connections_per_ip, 1);
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
    fn test_default_config_serializes_to_toml() {
        // `print-config` renders the resolved config as TOML, so the config must be
        // TOML-serializable.
        assert!(toml::to_string_pretty(&SeederConfig::default()).is_ok());
    }

    #[test]
    fn test_env_overrides() -> TestResult {
        let config = temp_env::with_var(
            "ZEBRA_SEEDER__DNS__DOMAIN",
            Some("test.example.com"),
            || SeederConfig::load_with_env(None),
        )?;
        assert_eq!(config.dns.domain, "test.example.com");
        Ok(())
    }

    #[test]
    fn flat_dns_env_key_is_rejected() {
        let config = temp_env::with_var("ZEBRA_SEEDER__DNS_TTL", Some("300"), || {
            SeederConfig::load_with_env(None)
        });

        assert!(config.is_err(), "flat DNS config key should fail");
    }

    #[test]
    fn upstream_network_env_key_is_rejected() {
        let config = temp_env::with_var("ZEBRA_SEEDER__NETWORK__NETWORK", Some("Testnet"), || {
            SeederConfig::load_with_env(None)
        });

        assert!(
            config.is_err(),
            "upstream zebra-network config key should fail"
        );
    }

    #[test]
    fn unknown_dns_env_key_is_rejected() {
        let config = temp_env::with_var("ZEBRA_SEEDER__DNS__TTTL", Some("300"), || {
            SeederConfig::load_with_env(None)
        });

        assert!(config.is_err(), "unknown DNS config key should fail");
    }

    #[test]
    fn invalid_dns_domain_is_rejected() {
        let config = temp_env::with_var("ZEBRA_SEEDER__DNS__DOMAIN", Some("not a domain"), || {
            SeederConfig::load_with_env(None)
        });

        assert!(config.is_err(), "invalid DNS domain should fail");
    }

    #[test]
    fn unknown_metrics_env_key_is_rejected() {
        let config = temp_env::with_var(
            "ZEBRA_SEEDER__METRICS__ENDPOINT_ADRR",
            Some("127.0.0.1:0"),
            || SeederConfig::load_with_env(None),
        );

        assert!(config.is_err(), "unknown metrics config key should fail");
    }

    #[test]
    fn unknown_rate_limit_env_key_is_rejected() {
        let config =
            temp_env::with_var("ZEBRA_SEEDER__RATE_LIMIT__BURSTT_SIZE", Some("100"), || {
                SeederConfig::load_with_env(None)
            });

        assert!(config.is_err(), "unknown rate-limit config key should fail");
    }

    #[test]
    fn zero_rate_limit_is_rejected() {
        let config = temp_env::with_var(
            "ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND",
            Some("0"),
            || SeederConfig::load_with_env(None),
        );

        assert!(config.is_err(), "zero query rate should fail");
    }

    #[test]
    fn zero_rate_limit_burst_is_rejected() {
        let config = temp_env::with_var("ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE", Some("0"), || {
            SeederConfig::load_with_env(None)
        });

        assert!(config.is_err(), "zero burst size should fail");
    }

    #[test]
    fn test_config_loading_from_env_overrides_network() -> TestResult {
        let config = temp_env::with_vars(
            [
                ("ZEBRA_SEEDER__CRAWLER__NETWORK", Some("Testnet")),
                ("ZEBRA_SEEDER__DNS__LISTEN_ADDR", Some("0.0.0.0:1053")),
            ],
            || SeederConfig::load_with_env(None),
        )?;
        assert_eq!(config.crawler.network, CrawlerNetwork::Testnet);
        assert_eq!(config.dns.listen_addr.port(), 1053);
        assert_eq!(
            config.crawler.network_config().listen_addr.port(),
            Network::new_default_testnet().default_port()
        );
        Ok(())
    }

    #[test]
    fn test_dns_ttl_from_env() -> TestResult {
        let config = temp_env::with_var("ZEBRA_SEEDER__DNS__TTL", Some("300"), || {
            SeederConfig::load_with_env(None)
        })?;
        assert_eq!(config.dns.ttl, 300);
        Ok(())
    }

    #[test]
    fn test_rate_limit_from_env() -> TestResult {
        let config = temp_env::with_vars(
            [
                ("ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND", Some("50")),
                ("ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE", Some("100")),
            ],
            || SeederConfig::load_with_env(None),
        )?;
        assert_eq!(
            config
                .rate_limit
                .map(|rl| (rl.queries_per_second, rl.burst_size)),
            Some((50, 100))
        );
        Ok(())
    }
}
