//! Peer-height probe task.
//!
//! Periodically opens isolated Zcash connections to peers in the
//! `AddressBook` to read each peer's reported `start_height` from the
//! `version` message. Results feed a [`ProbeMap`] (sync state ground truth)
//! and are published as a [`TipFilterSnapshot`] for the DNS-serving path.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use metrics::{counter, gauge, histogram};
use rand::{rng, seq::SliceRandom};
use tokio::{sync::watch, sync::Semaphore, task::JoinHandle};
use zebra_chain::parameters::Network;
use zebra_network::{AddressBook, PeerSocketAddr};

use crate::{
    config::TipFilterConfig,
    rpc_tip::RpcTipState,
    tip_filter::{compute_reference_tip, ProbeMap, TipFilterSnapshot, TipSource},
};

/// Maximum acceptable P25..P75 spread before we treat the network as
/// partitioned and refuse to publish a reference tip.
const MAX_TIP_SPREAD_BLOCKS: u32 = 20;

/// Spawn the probe task. Returns the JoinHandle and a watch receiver that the
/// DNS-serving path uses to read the current [`TipFilterSnapshot`].
pub fn spawn(
    config: TipFilterConfig,
    network: Network,
    user_agent: String,
    address_book: Arc<Mutex<AddressBook>>,
    rpc_tip: Option<RpcTipState>,
) -> (watch::Receiver<TipFilterSnapshot>, JoinHandle<()>) {
    let (tx, rx) = watch::channel(TipFilterSnapshot::disabled());

    let handle = tokio::spawn(async move {
        let probes = ProbeMap::new();
        let semaphore = Arc::new(Semaphore::new(config.probe_concurrency));
        let mut last_attempt: HashMap<PeerSocketAddr, Instant> = HashMap::new();

        let interval = Duration::from_secs(config.probe_interval_secs);
        let stale_after = Duration::from_secs(config.probe_stale_after_secs);
        let probe_timeout = Duration::from_secs(config.probe_timeout_secs);

        loop {
            run_cycle(
                &network,
                &user_agent,
                &address_book,
                &probes,
                &semaphore,
                probe_timeout,
                stale_after,
                &mut last_attempt,
            )
            .await;

            probes.prune_stale(stale_after);
            gauge!("seeder.probes.map_size").set(probes.len() as f64);

            publish_snapshot(&probes, &config, rpc_tip.as_ref(), stale_after, &tx);

            tokio::time::sleep(interval).await;
        }
    });

    (rx, handle)
}

#[allow(clippy::too_many_arguments)]
async fn run_cycle(
    network: &Network,
    user_agent: &str,
    address_book: &Arc<Mutex<AddressBook>>,
    probes: &ProbeMap,
    semaphore: &Arc<Semaphore>,
    probe_timeout: Duration,
    stale_after: Duration,
    last_attempt: &mut HashMap<PeerSocketAddr, Instant>,
) {
    let candidates = select_candidates(address_book, probes, last_attempt, stale_after);

    if candidates.is_empty() {
        tracing::debug!("probe cycle: no candidates");
        return;
    }

    tracing::info!(count = candidates.len(), "probe cycle: starting");

    let now = Instant::now();
    let mut joinset = tokio::task::JoinSet::new();
    for addr in candidates {
        last_attempt.insert(addr, now);
        let sem = semaphore.clone();
        let network = network.clone();
        let user_agent = user_agent.to_owned();
        let probes = probes.clone();
        joinset.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            gauge!("seeder.probes.in_flight").increment(1.0);
            counter!("seeder.probes.attempted_total").increment(1);
            let start = Instant::now();
            let result = tokio::time::timeout(
                probe_timeout,
                zebra_network::connect_isolated_tcp_direct(&network, addr, user_agent),
            )
            .await;
            histogram!("seeder.probes.handshake_latency_seconds")
                .record(start.elapsed().as_secs_f64());

            match result {
                Ok(Ok(client)) => {
                    let height = client.connection_info.remote.start_height.0;
                    let user_agent = client.connection_info.remote.user_agent.clone();
                    drop(client);
                    let accepted = probes.record_success(addr, height);
                    counter!("seeder.probes.succeeded_total").increment(1);
                    // `PeerSocketAddr`'s Display redacts the IP; deref for the real one.
                    tracing::debug!(
                        peer = %*addr,
                        height,
                        user_agent = %user_agent,
                        accepted,
                        "peer height observed",
                    );
                    if !accepted {
                        counter!("seeder.probes.failed_total", "reason" => "sanity_cap")
                            .increment(1);
                    }
                }
                Ok(Err(err)) => {
                    counter!("seeder.probes.failed_total", "reason" => "handshake").increment(1);
                    probes.record_failure(addr);
                    tracing::trace!(%addr, ?err, "probe handshake failed");
                }
                Err(_) => {
                    counter!("seeder.probes.failed_total", "reason" => "timeout").increment(1);
                    probes.record_failure(addr);
                    tracing::trace!(%addr, "probe timed out");
                }
            }
            gauge!("seeder.probes.in_flight").decrement(1.0);
        });
    }

    while joinset.join_next().await.is_some() {}
}

fn select_candidates(
    address_book: &Arc<Mutex<AddressBook>>,
    probes: &ProbeMap,
    last_attempt: &HashMap<PeerSocketAddr, Instant>,
    stale_after: Duration,
) -> Vec<PeerSocketAddr> {
    let guard = match address_book.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!("AddressBook mutex poisoned in probe scheduler, recovering");
            counter!("seeder.mutex_poisoning_total", "location" => "probe_scheduler").increment(1);
            poisoned.into_inner()
        }
    };

    let now = Instant::now();

    let mut never_probed: Vec<PeerSocketAddr> = Vec::new();
    let mut stale: Vec<PeerSocketAddr> = Vec::new();
    let mut backoff: Vec<PeerSocketAddr> = Vec::new();

    for meta in guard.peers() {
        let addr = meta.addr();

        // Skip non-routable addresses early.
        let ip = addr.ip();
        if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
            continue;
        }

        match probes.get(&addr) {
            None => never_probed.push(addr),
            Some(entry) => {
                if entry.consecutive_failures > 0 {
                    // Exponential backoff: skip until last_attempt + min(2^f, 32) * 60s.
                    let backoff_minutes = 1u64 << entry.consecutive_failures.min(5);
                    let backoff_dur = Duration::from_secs(backoff_minutes * 60);
                    if let Some(last) = last_attempt.get(&addr) {
                        if now.duration_since(*last) < backoff_dur {
                            continue;
                        }
                    }
                    backoff.push(addr);
                } else if now.duration_since(entry.sampled_at) > stale_after {
                    stale.push(addr);
                }
            }
        }
    }
    drop(guard);

    // Shuffle each priority class so we don't always probe in iteration order.
    never_probed.shuffle(&mut rng());
    stale.shuffle(&mut rng());
    backoff.shuffle(&mut rng());

    let mut out = Vec::with_capacity(never_probed.len() + stale.len() + backoff.len());
    out.extend(never_probed);
    out.extend(stale);
    out.extend(backoff);
    out
}

fn publish_snapshot(
    probes: &ProbeMap,
    config: &TipFilterConfig,
    rpc_tip: Option<&RpcTipState>,
    stale_after: Duration,
    tx: &watch::Sender<TipFilterSnapshot>,
) {
    let since = Instant::now() - stale_after;
    let samples = probes.fresh_heights(since);

    // Compute the probe-derived tip first; the RPC override (if fresh) wins.
    let computation =
        compute_reference_tip(&samples, config.min_probe_sample, MAX_TIP_SPREAD_BLOCKS);

    let rpc_fresh = rpc_tip.and_then(|s| s.read_fresh());
    let (reference_tip, source) = match (rpc_fresh, computation.reference_tip) {
        (Some(h), _) => (Some(h), TipSource::Rpc),
        (None, Some(h)) => (Some(h), TipSource::Probe),
        (None, None) => (None, TipSource::Unknown),
    };

    // Emit observability gauges regardless of whether a tip was published.
    gauge!("seeder.tip.sample_count").set(computation.sample_count as f64);
    gauge!("seeder.tip.p25_to_p75_spread").set(computation.spread as f64);
    set_source_gauge(source);
    if let Some(tip) = reference_tip {
        gauge!("seeder.tip.reference_height").set(tip as f64);
    }
    if computation.spread > MAX_TIP_SPREAD_BLOCKS {
        gauge!("seeder.tip.partition_detected").set(1.0);
    } else {
        gauge!("seeder.tip.partition_detected").set(0.0);
    }

    // Build the synced-peer set if we have a tip; emit offset histogram per peer.
    let mut synced_peers = std::collections::HashSet::new();
    let mut synced_v4_count = 0usize;
    let mut synced_v6_count = 0usize;
    if let Some(tip) = reference_tip {
        synced_peers = probes.synced_peers(tip, config.tip_tolerance_blocks, since);
        for addr in &synced_peers {
            match addr.ip() {
                IpAddr::V4(_) => synced_v4_count += 1,
                IpAddr::V6(_) => synced_v6_count += 1,
            }
        }
        // Record the height-offset histogram across all fresh probes.
        for h in &samples {
            let offset = *h as i64 - tip as i64;
            histogram!("seeder.probes.reported_height_offset").record(offset as f64);
        }
    }
    gauge!("seeder.peers.synced", "addr_family" => "v4").set(synced_v4_count as f64);
    gauge!("seeder.peers.synced", "addr_family" => "v6").set(synced_v6_count as f64);

    let snapshot = TipFilterSnapshot {
        reference_tip,
        synced_peers,
        synced_v4_count,
        synced_v6_count,
        sample_count: computation.sample_count,
        spread: computation.spread,
        source,
    };
    let _ = tx.send(snapshot);
}

fn set_source_gauge(source: TipSource) {
    let probe = if source == TipSource::Probe { 1.0 } else { 0.0 };
    let rpc = if source == TipSource::Rpc { 1.0 } else { 0.0 };
    let unknown = if source == TipSource::Unknown {
        1.0
    } else {
        0.0
    };
    gauge!("seeder.tip.source", "source" => "probe").set(probe);
    gauge!("seeder.tip.source", "source" => "rpc").set(rpc);
    gauge!("seeder.tip.source", "source" => "unknown").set(unknown);
}
