//! Layered configuration for the seeder: defaults, then an optional TOML file,
//! then `ZEBRA_SEEDER__*` environment variables (each layer overriding the last).

use color_eyre::eyre::Result;
use config::{Config, Environment, File};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Configuration for the Zebra seeder.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct SeederConfig {
    /// The Zebra network configuration.
    pub(crate) network: zebra_network::Config,

    /// The socket address Hickory DNS will bind to.
    ///
    /// Defaults to `0.0.0.0:53`.
    pub(crate) dns_listen_addr: SocketAddr,

    /// The domain name the seeder is authoritative for.
    pub(crate) seed_domain: String,

    /// DNS response TTL (time to live) in seconds.
    ///
    /// Controls how long clients cache DNS responses. Lower values mean fresher
    /// data but more queries; higher values reduce load but propagate updates
    /// more slowly. Defaults to `600` (10 minutes).
    pub(crate) dns_ttl: u32,

    /// Prometheus metrics configuration. If `None`, metrics are disabled.
    pub(crate) metrics: Option<MetricsConfig>,

    /// Rate limiting configuration. If `None`, rate limiting is disabled (not
    /// recommended for production).
    pub(crate) rate_limit: Option<RateLimitConfig>,
}

/// Configuration for Prometheus metrics.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
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
#[serde(default)]
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

impl Default for SeederConfig {
    fn default() -> Self {
        Self {
            network: zebra_network::Config::default(),
            dns_listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 53),
            seed_domain: "mainnet.seeder.example.com".to_string(),
            dns_ttl: 600,
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

        Ok(seeder_config)
    }
}
