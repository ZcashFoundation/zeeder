use std::{net::IpAddr, num::NonZeroU32, sync::Arc, time::Duration};

use governor::{clock, state::keyed::DashMapStateStore, Quota, RateLimiter as GovernorLimiter};
use tokio::task::JoinHandle;

use crate::config::SeederConfig;

pub type RateLimiter = GovernorLimiter<IpAddr, DashMapStateStore<IpAddr>, clock::DefaultClock>;

/// The interval at which to prune the map of rate limits by IP of entries with an effectively fresh state.
const RATE_LIMIT_PRUNE_INTERVAL: Duration = Duration::from_secs(5);

pub trait RateLimiterExt {
    fn new_map(queries_per_second: u32, burst_size: u32) -> Arc<Self>;
    fn check(&self, key: IpAddr) -> bool;
}

impl RateLimiterExt for RateLimiter {
    fn new_map(queries_per_second: u32, burst_size: u32) -> Arc<Self> {
        let quota = Quota::per_second(NonZeroU32::new(queries_per_second).unwrap())
            .allow_burst(NonZeroU32::new(burst_size).unwrap());
        Arc::new(Self::dashmap(quota))
    }

    fn check(&self, key: IpAddr) -> bool {
        self.check_key(&key).is_ok()
    }
}

pub fn spawn(config: SeederConfig) -> (Option<Arc<RateLimiter>>, JoinHandle<()>) {
    tracing::info!("Initializing zebra-network...");

    if let Some(cfg) = &config.rate_limit {
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
    } else {
        (None, tokio::spawn(std::future::pending()))
    }
}
