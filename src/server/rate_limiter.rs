//! Per-IP DNS query rate limiting, to prevent the seeder being used for DNS
//! amplification.

use std::{net::IpAddr, num::NonZeroU32, sync::Arc, time::Duration};

use governor::{Quota, RateLimiter as GovernorLimiter, clock, state::keyed::DashMapStateStore};
use tokio::task::JoinHandle;

use crate::config::SeederConfig;

pub(crate) type RateLimiter =
    GovernorLimiter<IpAddr, DashMapStateStore<IpAddr>, clock::DefaultClock>;

/// The interval at which to prune the map of rate limits by IP of entries with an effectively fresh state.
const RATE_LIMIT_PRUNE_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) trait RateLimiterExt {
    fn new_map(queries_per_second: u32, burst_size: u32) -> Arc<Self>;
    fn check(&self, key: IpAddr) -> bool;
}

impl RateLimiterExt for RateLimiter {
    fn new_map(queries_per_second: u32, burst_size: u32) -> Arc<Self> {
        // A configured 0 is treated as 1: zero queries per second is not a
        // meaningful limit, and this avoids a panic on a NonZero conversion.
        let quota =
            Quota::per_second(NonZeroU32::new(queries_per_second).unwrap_or(NonZeroU32::MIN))
                .allow_burst(NonZeroU32::new(burst_size).unwrap_or(NonZeroU32::MIN));
        Arc::new(Self::dashmap(quota))
    }

    fn check(&self, key: IpAddr) -> bool {
        self.check_key(&key).is_ok()
    }
}

pub(crate) fn spawn(config: &SeederConfig) -> (Option<Arc<RateLimiter>>, JoinHandle<()>) {
    config.rate_limit.as_ref().map_or_else(
        || (None, tokio::spawn(std::future::pending())),
        |cfg| {
            tracing::info!(
                "Rate limiting enabled: {} queries/sec per IP, burst size: {}",
                cfg.queries_per_second,
                cfg.burst_size
            );

            let limiter = RateLimiter::new_map(cfg.queries_per_second, cfg.burst_size);
            let prune_task = {
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(RATE_LIMIT_PRUNE_INTERVAL).await;
                        limiter.retain_recent();
                    }
                })
            };

            (Some(limiter), prune_task)
        },
    )
}
