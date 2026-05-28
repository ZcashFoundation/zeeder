use crate::config::SeederConfig;
use std::env;
use std::sync::Mutex;
use std::sync::OnceLock;

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn with_env_lock<F>(f: F)
where
    F: FnOnce(),
{
    let mutex = ENV_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = mutex.lock().unwrap_or_else(|e| e.into_inner());
    f();
}

#[test]
fn test_default_config() {
    let config = SeederConfig::default();
    assert_eq!(config.dns_listen_addr.to_string(), "0.0.0.0:53");
    assert_eq!(config.seed_domain, "mainnet.seeder.example.com");
    assert_eq!(config.dns_ttl, 600);
}

#[test]
fn test_env_overrides() {
    with_env_lock(|| {
        env::set_var("ZEBRA_SEEDER__SEED_DOMAIN", "test.example.com");

        let config = SeederConfig::load_with_env(None).expect("should load");

        assert_eq!(config.seed_domain, "test.example.com");

        // Clean up
        env::remove_var("ZEBRA_SEEDER__SEED_DOMAIN");
    });
}

#[test]
fn test_config_loading_from_env_overrides_network() {
    with_env_lock(|| {
        // Set environment variables
        std::env::set_var("ZEBRA_SEEDER__NETWORK__NETWORK", "Testnet");
        std::env::set_var("ZEBRA_SEEDER__DNS_LISTEN_ADDR", "0.0.0.0:1053");

        let config = SeederConfig::load_with_env(None).expect("should load");

        assert_eq!(config.network.network.to_string(), "Testnet");
        assert_eq!(config.dns_listen_addr.port(), 1053);

        // Clean up
        env::remove_var("ZEBRA_SEEDER__NETWORK__NETWORK");
        env::remove_var("ZEBRA_SEEDER__DNS_LISTEN_ADDR");
    });
}

#[test]
fn test_network_config_defaults() {
    // Verify default network config logic through SeederConfig
    let config = SeederConfig::default();
    // Zebra network default listening port depends on network, but here we check our config wrapper defaults
    // basic checks
    assert_eq!(config.network.network.to_string(), "Mainnet");
}

#[test]
fn test_dns_ttl_from_env() {
    with_env_lock(|| {
        env::set_var("ZEBRA_SEEDER__DNS_TTL", "300");

        let config = SeederConfig::load_with_env(None).expect("should load");

        assert_eq!(config.dns_ttl, 300);

        // Clean up
        env::remove_var("ZEBRA_SEEDER__DNS_TTL");
    });
}

#[test]
fn test_rate_limit_default() {
    let config = SeederConfig::default();
    assert!(config.rate_limit.is_some());
    let rl = config.rate_limit.unwrap();
    assert_eq!(rl.queries_per_second, 10);
    assert_eq!(rl.burst_size, 20);
}

#[test]
fn test_tip_filter_disabled_by_default() {
    let config = SeederConfig::default();
    assert!(config.tip_filter.is_none());
}

#[test]
fn test_tip_filter_enabled_via_env() {
    with_env_lock(|| {
        env::set_var("ZEBRA_SEEDER__TIP_FILTER__PROBE_CONCURRENCY", "8");
        env::set_var("ZEBRA_SEEDER__TIP_FILTER__TIP_TOLERANCE_BLOCKS", "12");

        let config = SeederConfig::load_with_env(None).expect("should load");

        let tip = config.tip_filter.expect("tip_filter should be Some");
        assert_eq!(tip.probe_concurrency, 8);
        assert_eq!(tip.tip_tolerance_blocks, 12);
        // Untouched fields keep their defaults
        assert_eq!(tip.min_synced_peers, 16);
        assert_eq!(tip.min_probe_sample, 8);

        env::remove_var("ZEBRA_SEEDER__TIP_FILTER__PROBE_CONCURRENCY");
        env::remove_var("ZEBRA_SEEDER__TIP_FILTER__TIP_TOLERANCE_BLOCKS");
    });
}

#[test]
fn test_rate_limit_from_env() {
    with_env_lock(|| {
        env::set_var("ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND", "50");
        env::set_var("ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE", "100");

        let config = SeederConfig::load_with_env(None).expect("should load");

        assert!(config.rate_limit.is_some());
        let rl = config.rate_limit.unwrap();
        assert_eq!(rl.queries_per_second, 50);
        assert_eq!(rl.burst_size, 100);

        // Clean up
        env::remove_var("ZEBRA_SEEDER__RATE_LIMIT__QUERIES_PER_SECOND");
        env::remove_var("ZEBRA_SEEDER__RATE_LIMIT__BURST_SIZE");
    });
}
