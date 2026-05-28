//! Chain-tip-aware peer filtering: pure types and logic (no I/O).
//!
//! The probe task (see [`crate::probe`]) feeds peer-reported chain heights
//! into a [`ProbeMap`]. The reference network tip is then computed as a
//! percentile of recent samples (with adversarial-input safeguards), and
//! individual peers are classified as Synced, Behind, or Unknown relative
//! to that reference. The DNS-serving path consumes a [`TipFilterSnapshot`]
//! over a `tokio::sync::watch` channel.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use zebra_network::PeerSocketAddr;

/// A single peer-height observation from a probe.
#[derive(Clone, Copy, Debug)]
pub struct ProbeEntry {
    /// Peer-reported `start_height` from the Zcash `version` message.
    pub height: u32,
    /// When the most recent successful probe completed.
    pub sampled_at: Instant,
    /// Consecutive probe failures since the last success (for backoff).
    pub consecutive_failures: u32,
}

/// Concurrent map of probe results keyed by peer address.
#[derive(Clone, Debug, Default)]
pub struct ProbeMap {
    entries: Arc<DashMap<PeerSocketAddr, ProbeEntry>>,
}

/// During bootstrap we accept the first few probes unconditionally because we
/// have no baseline to sanity-check against.
const SANITY_BOOTSTRAP_MIN_ENTRIES: usize = 3;
/// How many blocks above the current observed max a fresh probe may report
/// before we reject it as a poisoning attempt.
const SANITY_CAP_MAX_DELTA: u32 = 100;

impl ProbeMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record a successful probe. Returns `false` if the height was rejected
    /// by the sanity cap.
    pub fn record_success(&self, addr: PeerSocketAddr, height: u32) -> bool {
        // The sanity cap and bootstrap counter must consider *successful*
        // observations only. Failure stubs (height=0) would otherwise hold
        // the baseline at 0 and cause every real height to be rejected
        // until prune_stale clears them.
        let max_observed = self.successful_max_height();
        let bootstrap = self.successful_entry_count() < SANITY_BOOTSTRAP_MIN_ENTRIES;
        if !bootstrap && height > max_observed.saturating_add(SANITY_CAP_MAX_DELTA) {
            return false;
        }
        self.entries.insert(
            addr,
            ProbeEntry {
                height,
                sampled_at: Instant::now(),
                consecutive_failures: 0,
            },
        );
        true
    }

    /// Record a probe failure. Keeps the cached entry (with its old height)
    /// but bumps the failure counter for backoff scheduling.
    pub fn record_failure(&self, addr: PeerSocketAddr) {
        if let Some(mut entry) = self.entries.get_mut(&addr) {
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            return;
        }
        self.entries.insert(
            addr,
            ProbeEntry {
                height: 0,
                sampled_at: Instant::now() - Duration::from_secs(3600 * 24),
                consecutive_failures: 1,
            },
        );
    }

    /// Drop entries whose latest sample is older than `max_age`.
    pub fn prune_stale(&self, max_age: Duration) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.sampled_at) < max_age);
    }

    /// Heights of entries sampled at or after `since`.
    pub fn fresh_heights(&self, since: Instant) -> Vec<u32> {
        self.entries
            .iter()
            .filter(|kv| kv.value().sampled_at >= since && kv.value().consecutive_failures == 0)
            .map(|kv| kv.value().height)
            .collect()
    }

    /// Peers whose latest fresh probe puts them within `tolerance` of `tip`.
    pub fn synced_peers(
        &self,
        tip: u32,
        tolerance: u32,
        since: Instant,
    ) -> HashSet<PeerSocketAddr> {
        let floor = tip.saturating_sub(tolerance);
        self.entries
            .iter()
            .filter(|kv| {
                let e = kv.value();
                e.sampled_at >= since && e.consecutive_failures == 0 && e.height >= floor
            })
            .map(|kv| *kv.key())
            .collect()
    }

    pub fn get(&self, addr: &PeerSocketAddr) -> Option<ProbeEntry> {
        self.entries.get(addr).map(|e| *e.value())
    }

    fn successful_max_height(&self) -> u32 {
        self.entries
            .iter()
            .filter(|kv| kv.value().consecutive_failures == 0)
            .map(|kv| kv.value().height)
            .max()
            .unwrap_or(0)
    }

    fn successful_entry_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|kv| kv.value().consecutive_failures == 0)
            .count()
    }
}

/// Outcome of [`compute_reference_tip`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TipComputation {
    /// `Some` if at least `min_sample` fresh samples were available; `None`
    /// otherwise. `spread` is recorded separately for observability.
    pub reference_tip: Option<u32>,
    pub p25: u32,
    pub p75: u32,
    pub sample_count: usize,
    pub spread: u32,
}

/// Compute the reference tip from a set of fresh peer-reported heights.
///
/// Uses the 75th percentile (nearest-rank) for robustness against unsynced
/// peers and a small number of dishonest height claims. Returns
/// `reference_tip = None` only when `samples.len() < min_sample`.
///
/// `spread` (P75 − P25) is computed for observability but is **not** used to
/// gate the published tip: a typical seeder address book is full of
/// stragglers, so P25 lives far below tip and the resulting spread is large
/// by design. Real chain-partition detection needs a different statistic
/// (upper-cluster dispersion) and is out of scope here.
pub fn compute_reference_tip(samples: &[u32], min_sample: usize) -> TipComputation {
    if samples.len() < min_sample {
        return TipComputation {
            reference_tip: None,
            sample_count: samples.len(),
            ..Default::default()
        };
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let p25 = percentile(&sorted, 25);
    let p75 = percentile(&sorted, 75);
    let spread = p75.saturating_sub(p25);

    TipComputation {
        reference_tip: Some(p75),
        p25,
        p75,
        sample_count: sorted.len(),
        spread,
    }
}

/// Nearest-rank percentile on a sorted slice. `p` is a percentage in 0..=100.
fn percentile(sorted: &[u32], p: u32) -> u32 {
    debug_assert!(!sorted.is_empty());
    debug_assert!(p <= 100);
    let n = sorted.len() as u64;
    let p = p as u64;
    let rank = (n * p).div_ceil(100);
    let idx = (rank.saturating_sub(1) as usize).min(sorted.len() - 1);
    sorted[idx]
}

/// Classification of a single peer relative to the reference tip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerSyncStatus {
    Synced,
    Behind,
    Unknown,
}

/// Classify a peer using the probe map and the current reference tip.
pub fn classify_peer(
    addr: PeerSocketAddr,
    probes: &ProbeMap,
    tip: Option<u32>,
    tolerance: u32,
    fresh_since: Instant,
) -> PeerSyncStatus {
    let Some(tip) = tip else {
        return PeerSyncStatus::Unknown;
    };
    let Some(entry) = probes.get(&addr) else {
        return PeerSyncStatus::Unknown;
    };
    if entry.sampled_at < fresh_since || entry.consecutive_failures > 0 {
        return PeerSyncStatus::Unknown;
    }
    let floor = tip.saturating_sub(tolerance);
    if entry.height >= floor {
        PeerSyncStatus::Synced
    } else {
        PeerSyncStatus::Behind
    }
}

/// Where the published reference tip came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TipSource {
    #[default]
    Unknown,
    Probe,
    Rpc,
}

/// Snapshot of the tip filter state, published from the probe task to the
/// DNS-serving path via `tokio::sync::watch`.
#[derive(Clone, Debug, Default)]
pub struct TipFilterSnapshot {
    pub reference_tip: Option<u32>,
    pub synced_peers: HashSet<PeerSocketAddr>,
    pub synced_v4_count: usize,
    pub synced_v6_count: usize,
    pub sample_count: usize,
    pub spread: u32,
    pub source: TipSource,
}

impl TipFilterSnapshot {
    pub fn disabled() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::str::FromStr;

    fn addr(last: u8) -> PeerSocketAddr {
        PeerSocketAddr::from(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::new(1, 2, 3, last)),
            8233,
        ))
    }

    // ---------- compute_reference_tip ----------

    #[test]
    fn reference_tip_none_below_min_sample() {
        let result = compute_reference_tip(&[100, 200, 300], 8);
        assert_eq!(result.reference_tip, None);
        assert_eq!(result.sample_count, 3);
    }

    #[test]
    fn reference_tip_equals_value_when_all_identical() {
        let samples = vec![1_500_000_u32; 16];
        let result = compute_reference_tip(&samples, 8);
        assert_eq!(result.reference_tip, Some(1_500_000));
        assert_eq!(result.spread, 0);
    }

    #[test]
    fn reference_tip_rejects_single_poison_sample() {
        // 99 honest peers at height 1_000_000, 1 malicious at u32::MAX.
        let mut samples = vec![1_000_000_u32; 99];
        samples.push(u32::MAX);
        let result = compute_reference_tip(&samples, 8);
        // P75 of (99 × 1_000_000 + 1 × u32::MAX) is still 1_000_000.
        assert_eq!(result.reference_tip, Some(1_000_000));
    }

    #[test]
    fn reference_tip_published_despite_wide_spread() {
        // Realistic seeder mix: laggards from height ~1k and synced peers near
        // tip. We must still publish a tip; the spread is informational only.
        let mut samples = vec![1_000_u32; 8];
        samples.extend(vec![3_358_326_u32; 8]);
        let result = compute_reference_tip(&samples, 8);
        assert_eq!(
            result.reference_tip,
            Some(3_358_326),
            "P75 sits in upper cluster"
        );
        assert!(
            result.spread > 1_000_000,
            "spread reported for observability"
        );
    }

    // ---------- classify_peer ----------

    #[test]
    fn classify_unknown_when_tip_is_none() {
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 1_000_000);
        let now = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            classify_peer(addr(1), &probes, None, 8, now),
            PeerSyncStatus::Unknown
        );
    }

    #[test]
    fn classify_unknown_when_peer_not_probed() {
        let probes = ProbeMap::new();
        let now = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            classify_peer(addr(1), &probes, Some(1_000_000), 8, now),
            PeerSyncStatus::Unknown
        );
    }

    #[test]
    fn classify_synced_at_tolerance_boundary() {
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 999_992); // exactly tip - tolerance
        let now = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            classify_peer(addr(1), &probes, Some(1_000_000), 8, now),
            PeerSyncStatus::Synced
        );
    }

    #[test]
    fn classify_behind_one_block_past_tolerance() {
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 999_991); // tip - tolerance - 1
        let now = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            classify_peer(addr(1), &probes, Some(1_000_000), 8, now),
            PeerSyncStatus::Behind
        );
    }

    #[test]
    fn classify_synced_when_slightly_ahead() {
        // A peer reporting one block ahead of the reference tip (probe race)
        // should still be considered synced.
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 1_000_001);
        let now = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            classify_peer(addr(1), &probes, Some(1_000_000), 8, now),
            PeerSyncStatus::Synced
        );
    }

    // ---------- ProbeMap sanity cap ----------

    #[test]
    fn sanity_cap_rejects_outlier_after_bootstrap() {
        let probes = ProbeMap::new();
        // Bootstrap with honest values
        probes.record_success(addr(1), 1_000_000);
        probes.record_success(addr(2), 1_000_001);
        probes.record_success(addr(3), 1_000_002);
        probes.record_success(addr(4), 1_000_003);
        // Outlier well above max + 100 must be rejected
        let accepted = probes.record_success(addr(5), 5_000_000);
        assert!(!accepted, "outlier should be rejected");
        assert!(probes.get(&addr(5)).is_none());
    }

    #[test]
    fn sanity_cap_skipped_during_bootstrap() {
        let probes = ProbeMap::new();
        let accepted = probes.record_success(addr(1), 1_500_000);
        assert!(accepted);
        assert_eq!(probes.get(&addr(1)).map(|e| e.height), Some(1_500_000));
    }

    #[test]
    fn failure_stubs_do_not_break_sanity_cap_baseline() {
        // Regression: when many probes fail before any succeed in a cycle,
        // failure stubs (height=0) used to dominate max_height(), causing
        // the first real probe to be rejected by the sanity cap. The fix
        // is to consider only successful entries in the cap's baseline.
        let probes = ProbeMap::new();
        for i in 0..10 {
            probes.record_failure(addr(i));
        }
        // First real probe arrives. Without the fix this returned `false`
        // because 3_358_315 > 0 + 100. With the fix bootstrap is still
        // active (no successful entries yet) and it's accepted.
        let accepted = probes.record_success(addr(100), 3_358_315);
        assert!(
            accepted,
            "real height must not be rejected just because the map contains failure stubs"
        );
        // Subsequent honest heights should also be accepted.
        assert!(probes.record_success(addr(101), 3_358_316));
        assert!(probes.record_success(addr(102), 3_358_317));
        assert!(probes.record_success(addr(103), 3_358_318));
        // A real outlier is still rejected once we have a real baseline.
        assert!(!probes.record_success(addr(104), 10_000_000));
    }

    #[test]
    fn record_failure_increments_counter() {
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 1_000_000);
        probes.record_failure(addr(1));
        let entry = probes.get(&addr(1)).expect("entry exists");
        assert_eq!(entry.consecutive_failures, 1);
        probes.record_failure(addr(1));
        let entry = probes.get(&addr(1)).expect("entry exists");
        assert_eq!(entry.consecutive_failures, 2);
    }

    #[test]
    fn classify_unknown_when_consecutive_failures_present() {
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 1_000_000);
        probes.record_failure(addr(1));
        let now = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            classify_peer(addr(1), &probes, Some(1_000_000), 8, now),
            PeerSyncStatus::Unknown
        );
    }

    #[test]
    fn prune_drops_stale_entries() {
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 1_000_000);
        std::thread::sleep(Duration::from_millis(20));
        probes.prune_stale(Duration::from_millis(10));
        assert_eq!(probes.len(), 0);
    }

    #[test]
    fn synced_peers_filters_by_floor_and_freshness() {
        let probes = ProbeMap::new();
        probes.record_success(addr(1), 1_000_000); // synced
        probes.record_success(addr(2), 999_990); // behind
        probes.record_success(addr(3), 1_000_005); // synced
        let since = Instant::now() - Duration::from_secs(1);
        let synced = probes.synced_peers(1_000_000, 8, since);
        assert!(synced.contains(&addr(1)));
        assert!(!synced.contains(&addr(2)));
        assert!(synced.contains(&addr(3)));
    }

    // ---------- Property tests ----------

    proptest! {
        /// Adding a strictly-larger sample never lowers P75.
        #[test]
        fn reference_tip_monotonic_in_max(
            base_samples in prop::collection::vec(0u32..1_000_000, 8..50),
            extra in 0u32..1_000_000_000,
        ) {
            let base_result = compute_reference_tip(&base_samples, 8);
            let mut augmented = base_samples.clone();
            augmented.push(extra);
            let aug_result = compute_reference_tip(&augmented, 8);
            if let (Some(b), Some(a)) = (base_result.reference_tip, aug_result.reference_tip) {
                if extra >= b {
                    prop_assert!(a >= b, "P75 must not decrease when adding a larger sample");
                }
            }
        }

        /// The single-poison case: at most one bogus huge height can never
        /// move P75 by more than SANITY_CAP_MAX_DELTA above the honest max,
        /// because ProbeMap::record_success refuses to insert it.
        #[test]
        fn sanity_cap_bounds_single_poison(
            honest in prop::collection::vec(1_000u32..1_100_000, 4..50),
        ) {
            let probes = ProbeMap::new();
            for (i, h) in honest.iter().enumerate() {
                probes.record_success(addr(i as u8), *h);
            }
            let honest_max = *honest.iter().max().unwrap();
            // Try to poison with a max-value attempt
            let _ = probes.record_success(addr(255), u32::MAX);
            let since = Instant::now() - Duration::from_secs(60);
            let fresh = probes.fresh_heights(since);
            let observed_max = fresh.iter().copied().max().unwrap_or(0);
            prop_assert!(observed_max <= honest_max.saturating_add(SANITY_CAP_MAX_DELTA),
                "single probe must not push observed max more than +100 above honest max");
        }
    }

    // Help: PeerSocketAddr From<SocketAddr> reachability check
    #[test]
    fn peer_socket_addr_construction_works() {
        let _ = PeerSocketAddr::from_str("127.0.0.1:8233").unwrap();
    }
}
