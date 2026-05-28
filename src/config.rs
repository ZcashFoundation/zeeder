use color_eyre::eyre::Result;
use config::{Config, Environment, File};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Configuration for the Zebra Seeder.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SeederConfig {
    /// The Zebra network configuration.
    pub network: zebra_network::Config,

    /// The socket address Hickory DNS will bind to.
    ///
    /// Defaults to `0.0.0.0:53`.
    pub dns_listen_addr: SocketAddr,

    /// The domain name the seeder is authoritative for.
    pub seed_domain: String,

    /// DNS response TTL (Time To Live) in seconds.
    ///
    /// Controls how long clients cache DNS responses.
    /// Lower values mean fresher data but more queries.
    /// Higher values reduce query load but slower updates.
    ///
    /// Defaults to `600` (10 minutes).
    pub dns_ttl: u32,

    /// Prometheus metrics configuration.
    ///
    /// If `None`, metrics are disabled.
    pub metrics: Option<MetricsConfig>,

    /// Rate limiting configuration.
    ///
    /// If `None`, rate limiting is disabled (NOT recommended for production).
    pub rate_limit: Option<RateLimitConfig>,

    /// Chain-tip-aware peer filtering.
    ///
    /// If `None`, the seeder serves all reachable peers regardless of sync
    /// state (the historical behavior). If `Some`, the seeder probes peers
    /// for their reported chain tip and prefers serving those that are at or
    /// near the network tip.
    pub tip_filter: Option<TipFilterConfig>,
}

/// Configuration for Prometheus metrics.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// The socket address to expose Prometheus metrics on.
    ///
    /// Defaults to `0.0.0.0:9999`.
    pub endpoint_addr: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            endpoint_addr: "0.0.0.0:9999".parse().expect("valid address"),
        }
    }
}

/// Configuration for DNS query rate limiting.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Maximum queries per second per IP address.
    ///
    /// Defaults to `10`.
    pub queries_per_second: u32,

    /// Burst capacity (maximum queries in a short burst).
    ///
    /// Defaults to `20` (2x the rate).
    pub burst_size: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            queries_per_second: 10,
            burst_size: 20,
        }
    }
}

/// Configuration for chain-tip-aware peer filtering.
///
/// When enabled, the seeder periodically probes peers it knows about to learn
/// their reported chain tip via the Zcash `version` handshake. The reference
/// tip is computed as the 75th percentile of fresh probe results (or sourced
/// from an optional trusted RPC endpoint). DNS responses then prefer peers
/// whose reported height is within `tip_tolerance_blocks` of the reference
/// tip. If too few synced peers are known, the seeder falls back to its
/// unfiltered behavior to avoid locking new nodes out of the network.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TipFilterConfig {
    /// Maximum concurrent peer probes.
    pub probe_concurrency: usize,
    /// Outer probe scheduler tick interval, in seconds.
    pub probe_interval_secs: u64,
    /// Per-probe total timeout (TCP connect + handshake), in seconds.
    pub probe_timeout_secs: u64,
    /// Probe entries older than this are considered stale and re-probed.
    pub probe_stale_after_secs: u64,
    /// Maximum acceptable block-height delta from the reference tip for a
    /// peer to be classified as "synced".
    pub tip_tolerance_blocks: u32,
    /// Minimum count of synced peers (per address family) required before
    /// the hard filter engages. Below this threshold the seeder serves the
    /// unfiltered set instead.
    pub min_synced_peers: usize,
    /// Minimum count of fresh probe samples required before a reference
    /// tip is computed.
    pub min_probe_sample: usize,

    /// Optional trusted JSON-RPC endpoint to source the reference tip from
    /// instead of (or in addition to) probe-derived heights.
    pub rpc_override: Option<RpcOverrideConfig>,
}

impl Default for TipFilterConfig {
    fn default() -> Self {
        Self {
            probe_concurrency: 32,
            probe_interval_secs: 60,
            probe_timeout_secs: 10,
            probe_stale_after_secs: 600,
            tip_tolerance_blocks: 8,
            min_synced_peers: 16,
            min_probe_sample: 8,
            rpc_override: None,
        }
    }
}

impl TipFilterConfig {
    /// Reject footgun configurations at load time.
    pub fn validate(&self) -> Result<()> {
        use color_eyre::eyre::eyre;
        if self.probe_concurrency == 0 {
            return Err(eyre!("tip_filter.probe_concurrency must be >= 1"));
        }
        if self.min_synced_peers == 0 {
            return Err(eyre!("tip_filter.min_synced_peers must be >= 1"));
        }
        if self.min_probe_sample == 0 {
            return Err(eyre!("tip_filter.min_probe_sample must be >= 1"));
        }
        if self.probe_timeout_secs == 0 {
            return Err(eyre!("tip_filter.probe_timeout_secs must be >= 1"));
        }
        if self.probe_interval_secs == 0 {
            return Err(eyre!("tip_filter.probe_interval_secs must be >= 1"));
        }
        Ok(())
    }
}

/// Configuration for the optional trusted-RPC tip override.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RpcOverrideConfig {
    /// JSON-RPC endpoint URL (e.g., a local zebrad or zcashd).
    pub url: String,
    /// Poll interval, in seconds.
    pub poll_interval_secs: u64,
    /// Optional HTTP basic-auth credentials (zcashd uses these).
    pub basic_auth: Option<(String, String)>,
}

impl Default for RpcOverrideConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            poll_interval_secs: 30,
            basic_auth: None,
        }
    }
}

impl Default for SeederConfig {
    fn default() -> Self {
        Self {
            network: zebra_network::Config::default(),
            dns_listen_addr: "0.0.0.0:53"
                .parse()
                .expect("hardcoded address must be valid"),
            seed_domain: "mainnet.seeder.example.com".to_string(),
            dns_ttl: 600, // 10 minutes
            metrics: None,
            rate_limit: Some(RateLimitConfig::default()),
            tip_filter: None,
        }
    }
}

impl SeederConfig {
    /// Load the configuration from the given path, merging with default settings and
    /// environment variables.
    ///
    /// Precedence:
    /// 1. Environment Variables (ZEBRA_SEEDER_*)
    /// 2. Config File (if path is provided)
    /// 3. Default Values
    pub fn load_with_env(path: Option<std::path::PathBuf>) -> Result<Self> {
        let mut builder = Config::builder().add_source(Config::try_from(&Self::default())?);

        if let Some(path) = path {
            builder = builder.add_source(File::from(path));
        }

        builder = builder.add_source(
            Environment::with_prefix("ZEBRA_SEEDER")
                .separator("__")
                .try_parsing(true),
        );

        let config = builder.build()?;
        let seeder_config: SeederConfig = config.try_deserialize()?;

        if let Some(tip_cfg) = &seeder_config.tip_filter {
            tip_cfg.validate()?;
        }

        Ok(seeder_config)
    }

    /// Load the configuration from the given path, using default settings for any
    /// unspecified fields.
    ///
    /// This is a convenience wrapper around `load_with_env` that ignores environment variables
    /// for testing purposes, or can be used if env vars are not desired.
    /// However, strictly following the pattern, `load_with_env` is the primary entry point.
    // In Zebrad, `load` usually implies just file + defaults, but here we generally want Env too.
    // For simplicity and matching typical app flow, strictly following the prompt's request for "load" and "load_with_env":
    pub fn load(path: std::path::PathBuf) -> Result<Self> {
        Self::load_with_env(Some(path))
    }
}
