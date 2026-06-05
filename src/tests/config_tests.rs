use crate::config::SeederConfig;

type TestResult = color_eyre::Result<()>;

#[test]
fn test_default_config() {
    let config = SeederConfig::default();
    assert_eq!(config.dns_listen_addr.to_string(), "0.0.0.0:53");
    assert_eq!(config.seed_domain, "mainnet.seeder.example.com");
    assert_eq!(config.dns_ttl, 600);
}

#[test]
fn test_network_config_defaults() {
    let config = SeederConfig::default();
    assert_eq!(config.network.network.to_string(), "Mainnet");
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
        "ZEBRA_SEEDER__SEED_DOMAIN",
        Some("test.example.com"),
        || SeederConfig::load_with_env(None),
    )?;
    assert_eq!(config.seed_domain, "test.example.com");
    Ok(())
}

#[test]
fn test_config_loading_from_env_overrides_network() -> TestResult {
    let config = temp_env::with_vars(
        [
            ("ZEBRA_SEEDER__NETWORK__NETWORK", Some("Testnet")),
            ("ZEBRA_SEEDER__DNS_LISTEN_ADDR", Some("0.0.0.0:1053")),
        ],
        || SeederConfig::load_with_env(None),
    )?;
    assert_eq!(config.network.network.to_string(), "Testnet");
    assert_eq!(config.dns_listen_addr.port(), 1053);
    Ok(())
}

#[test]
fn test_dns_ttl_from_env() -> TestResult {
    let config = temp_env::with_var("ZEBRA_SEEDER__DNS_TTL", Some("300"), || {
        SeederConfig::load_with_env(None)
    })?;
    assert_eq!(config.dns_ttl, 300);
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
