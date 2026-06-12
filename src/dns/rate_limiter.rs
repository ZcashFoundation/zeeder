//! Per-IP DNS query rate limiting, to prevent the seeder being used for DNS
//! amplification.

use std::{net::IpAddr, sync::Arc, time::Duration};

use color_eyre::eyre::Result;
use governor::{Quota, RateLimiter as GovernorLimiter, clock, state::keyed::DashMapStateStore};

use crate::config::RateLimitConfig;

type GovernorRateLimiter = GovernorLimiter<IpAddr, DashMapStateStore<IpAddr>, clock::DefaultClock>;

/// How often stale per-IP rate-limit entries are pruned from the map.
const RATE_LIMIT_PRUNE_INTERVAL: Duration = Duration::from_secs(5);

/// Per-IP DNS query rate limiter.
#[derive(Clone)]
pub(crate) struct RateLimiter {
    limiter: Arc<GovernorRateLimiter>,
}

impl RateLimiter {
    /// Create a rate limiter and start its detached stale-entry prune loop.
    pub(crate) fn new(config: &RateLimitConfig) -> Result<Self> {
        let queries_per_second = config.nonzero_queries_per_second()?;
        let burst_size = config.nonzero_burst_size()?;

        tracing::info!(
            "Rate limiting enabled: {} queries/sec per IP, burst size: {}",
            queries_per_second,
            burst_size
        );

        let quota = Quota::per_second(queries_per_second).allow_burst(burst_size);

        let limiter = Arc::new(GovernorRateLimiter::dashmap(quota));
        detach_prune_loop(limiter.clone());

        Ok(Self { limiter })
    }

    /// Return whether `key` is within its configured query budget.
    pub(crate) fn check(&self, key: IpAddr) -> bool {
        self.limiter.check_key(&key).is_ok()
    }
}

fn detach_prune_loop(limiter: Arc<GovernorRateLimiter>) {
    let prune_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(RATE_LIMIT_PRUNE_INTERVAL).await;
            limiter.retain_recent();
        }
    });
    std::mem::drop(prune_task);
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    type TestResult = color_eyre::Result<()>;

    fn rate_limit_config(queries_per_second: u32, burst_size: u32) -> RateLimitConfig {
        RateLimitConfig {
            queries_per_second,
            burst_size,
        }
    }

    #[tokio::test]
    async fn test_rate_limiter_allows_normal_queries() -> TestResult {
        let limiter = RateLimiter::new(&rate_limit_config(10, 20))?;
        let test_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        assert!(limiter.check(test_ip), "first query should be allowed");
        assert!(
            limiter.check(test_ip),
            "second query should be allowed within burst"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_excessive_queries() -> TestResult {
        let limiter = RateLimiter::new(&rate_limit_config(1, 2))?;
        let test_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));

        assert!(limiter.check(test_ip), "Query 1 should pass");
        assert!(limiter.check(test_ip), "Query 2 should pass");
        assert!(!limiter.check(test_ip), "Query 3 should be rate limited");
        Ok(())
    }

    #[tokio::test]
    async fn test_rate_limiter_per_ip_isolation() -> TestResult {
        let limiter = RateLimiter::new(&rate_limit_config(1, 1))?;
        let ip1 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));

        assert!(limiter.check(ip1), "IP1 first query should pass");
        assert!(!limiter.check(ip1), "IP1 second query should be blocked");
        assert!(limiter.check(ip2), "IP2 should have independent quota");
        Ok(())
    }

    #[tokio::test]
    async fn test_rate_limiter_ipv6_support() -> TestResult {
        let limiter = RateLimiter::new(&rate_limit_config(10, 20))?;
        let ipv6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));

        assert!(limiter.check(ipv6), "IPv6 addresses should be supported");
        Ok(())
    }

    #[tokio::test]
    async fn zero_rate_limit_config_is_rejected() {
        let limiter = RateLimiter::new(&rate_limit_config(0, 20));

        assert!(limiter.is_err(), "zero query rate should fail");
    }
}
