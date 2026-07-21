//! Independent network-upgrade activation observation.
//!
//! The observer samples peers already proven live by zebra-network, performs a
//! direct isolated handshake with one peer per network group, and advances the
//! crawler's protocol floor only after a fixed quorum reports the compiled
//! activation safely buried for consecutive sweeps. Peer-reported heights are
//! not authenticated, so this raises the cost of a false decision but cannot
//! make one impossible under a group-supermajority Sybil or full eclipse.

use std::{
    collections::HashSet,
    io,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Utc;
use metrics::gauge;
use rand::{rng, seq::SliceRandom};
use tokio::{task::JoinHandle, task::JoinSet, time::MissedTickBehavior};
use zebra_chain::{
    block::Height,
    common::atomic_write,
    parameters::{Network, NetworkUpgrade, constants::MAX_BLOCK_REORG_HEIGHT},
};
use zebra_network::{
    AddressBook, PeerSocketAddr, Version, config::CacheDir, constants::HANDSHAKE_TIMEOUT,
    types::PeerServices,
};

use crate::{
    config::ZcashNetwork,
    crawl::{chain_tip::SeederChainTip, servability::classify_peer},
    metrics::{LABEL_NETWORK, MIN_PROTOCOL_VERSION},
};

/// Minimum independent network groups required for an activation decision.
const MIN_NETWORK_GROUPS: usize = 12;

/// Fixed supermajority required in each sweep.
const QUORUM_NUMERATOR: usize = 3;
const QUORUM_DENOMINATOR: usize = 4;

/// Number of consecutive qualifying sweeps required before confirmation.
const REQUIRED_QUALIFYING_SWEEPS: u8 = 3;

/// Allow both the TCP connection and version handshake to complete.
const OBSERVATION_TIMEOUT: Duration = HANDSHAKE_TIMEOUT.saturating_mul(2);

/// The newest compiled activation and the evidence required to confirm it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActivationTarget {
    pub(crate) activation_height: Height,
    pub(crate) confirmation_height: Height,
    pub(crate) pre_activation_height: Height,
    pub(crate) required_version: Version,
}

/// Owns the activation task and aborts it when its network crawler stops.
#[derive(Debug)]
pub(crate) struct ActivationObserver(JoinHandle<()>);

impl Drop for ActivationObserver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ActivationTarget {
    /// Build the observation target from Zebra's compiled activation table.
    pub(crate) fn latest(network: &Network) -> Self {
        let (_upgrade, activation_height) =
            NetworkUpgrade::current_with_activation_height(network, Height::MAX);
        let confirmation_height =
            Height(activation_height.0.saturating_add(MAX_BLOCK_REORG_HEIGHT));
        let pre_activation_height = activation_height.previous().unwrap_or(Height(0));
        let required_version = Version::min_remote_for_height(network, activation_height);

        Self {
            activation_height,
            confirmation_height,
            pre_activation_height,
            required_version,
        }
    }

    fn confirmation_record(self) -> String {
        format!(
            "activation_height={}\nconfirmation_height={}\nminimum_protocol_version={}\n",
            self.activation_height.0, self.confirmation_height.0, self.required_version.0
        )
    }

    fn matches_confirmation_record(self, record: &str) -> bool {
        record == self.confirmation_record()
    }
}

/// Return the persisted confirmation path beside zebra-network's peer cache.
pub(crate) fn confirmation_path(cache_dir: &CacheDir, network: &Network) -> Option<PathBuf> {
    cache_dir.cache_dir().map(|cache_dir| {
        cache_dir
            .join("network")
            .join(format!("{}.activation", network.lowercase_name()))
    })
}

/// Return whether the persisted record confirms this exact compiled target.
pub(crate) async fn load_confirmation(path: Option<&Path>, target: ActivationTarget) -> bool {
    let Some(path) = path else {
        return false;
    };

    match tokio::fs::read_to_string(path).await {
        Ok(record) if target.matches_confirmation_record(&record) => true,
        Ok(_) => {
            tracing::warn!(
                path = %path.display(),
                "ignoring activation confirmation that does not match the compiled target"
            );
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to read activation confirmation; observing activation again"
            );
            false
        }
    }
}

/// Spawn the independent activation observer for one network.
pub(crate) fn spawn(
    address_book: Arc<Mutex<AddressBook>>,
    network: ZcashNetwork,
    user_agent: String,
    tip: SeederChainTip,
    target: ActivationTarget,
    confirmation_path: Option<PathBuf>,
) -> ActivationObserver {
    ActivationObserver(tokio::spawn(async move {
        if tip.is_activation_confirmed() {
            return;
        }

        let zcash_network = network.to_zebra();
        let network_label = network.label();
        let sweep_interval =
            NetworkUpgrade::target_spacing_for_height(&zcash_network, target.activation_height)
                .to_std()
                .unwrap_or(Duration::from_secs(75));
        let mut interval = tokio::time::interval(sweep_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut gate = ActivationGate::default();

        loop {
            interval.tick().await;

            let sampled_peers = sample_network_groups(
                &address_book,
                &zcash_network,
                Version::min_remote_for_height(&zcash_network, target.pre_activation_height),
            );
            let evidence = observe_sweep(
                sampled_peers,
                zcash_network.clone(),
                user_agent.clone(),
                target,
            )
            .await;
            let confirmed = gate.observe(evidence);

            tracing::info!(
                network = network_label,
                total_groups = evidence.total_groups,
                ready_groups = evidence.ready_groups,
                qualifying_sweeps = gate.qualifying_sweeps(),
                required_groups = MIN_NETWORK_GROUPS,
                required_sweeps = REQUIRED_QUALIFYING_SWEEPS,
                quorum_numerator = QUORUM_NUMERATOR,
                quorum_denominator = QUORUM_DENOMINATOR,
                activation_height = target.activation_height.0,
                confirmation_height = target.confirmation_height.0,
                required_version = target.required_version.0,
                "activation observation sweep"
            );

            if !confirmed {
                continue;
            }

            if let Err(error) = persist_confirmation(confirmation_path.clone(), target).await {
                tracing::error!(
                    network = network_label,
                    %error,
                    "refusing to raise protocol floor without durable activation confirmation"
                );
                continue;
            }

            tip.confirm_activation();
            gauge!(MIN_PROTOCOL_VERSION, LABEL_NETWORK => network_label)
                .set(f64::from(target.required_version.0));
            tracing::info!(
                network = network_label,
                activation_height = target.activation_height.0,
                confirmation_height = target.confirmation_height.0,
                required_version = target.required_version.0,
                "activation confirmed; raised peer protocol-version floor"
            );
            break;
        }
    }))
}

/// One completed observer sweep, reduced to the values used by the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SweepEvidence {
    total_groups: usize,
    ready_groups: usize,
}

/// Fixed-quorum activation state machine.
#[derive(Debug, Default)]
struct ActivationGate {
    qualifying_sweeps: u8,
}

/// Stable fallback grouping that limits raw-IP Sybil weight.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum NetworkGroup {
    Ipv4([u8; 2]),
    Ipv6([u16; 2]),
}

fn network_group(ip: IpAddr) -> NetworkGroup {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            NetworkGroup::Ipv4([octets[0], octets[1]])
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            NetworkGroup::Ipv6([segments[0], segments[1]])
        }
    }
}

impl ActivationGate {
    /// Record one complete sweep and return whether activation is confirmed.
    fn observe(&mut self, evidence: SweepEvidence) -> bool {
        let has_enough_groups = evidence.total_groups >= MIN_NETWORK_GROUPS;
        let has_fixed_quorum = evidence.ready_groups.saturating_mul(QUORUM_DENOMINATOR)
            >= evidence.total_groups.saturating_mul(QUORUM_NUMERATOR);

        if !has_enough_groups || !has_fixed_quorum {
            self.qualifying_sweeps = 0;
            return false;
        }

        self.qualifying_sweeps = self.qualifying_sweeps.saturating_add(1);
        self.qualifying_sweeps >= REQUIRED_QUALIFYING_SWEEPS
    }

    fn qualifying_sweeps(&self) -> u8 {
        self.qualifying_sweeps
    }
}

fn sample_network_groups(
    address_book: &Mutex<AddressBook>,
    network: &Network,
    minimum_version: Version,
) -> Vec<PeerSocketAddr> {
    let mut candidates = {
        let book = match address_book.lock() {
            Ok(book) => book,
            Err(poisoned) => {
                tracing::error!("address book mutex poisoned during activation sampling");
                poisoned.into_inner()
            }
        };
        let now = Utc::now();

        book.peers()
            .filter(|meta| classify_peer(meta, now, network, minimum_version).is_ok())
            .map(|meta| meta.addr())
            .collect::<Vec<_>>()
    };

    candidates.shuffle(&mut rng());
    let mut sampled_groups = HashSet::new();
    candidates.retain(|addr| sampled_groups.insert(network_group(addr.ip())));
    candidates
}

async fn observe_sweep(
    sampled_peers: Vec<PeerSocketAddr>,
    network: Network,
    user_agent: String,
    target: ActivationTarget,
) -> SweepEvidence {
    let total_groups = sampled_peers.len();
    let mut probes = JoinSet::new();

    for addr in sampled_peers {
        let network = network.clone();
        let user_agent = user_agent.clone();
        probes.spawn(async move { observe_peer(network, addr, user_agent, target).await });
    }

    let mut ready_groups = 0;
    while let Some(probe_outcome) = probes.join_next().await {
        if matches!(probe_outcome, Ok(true)) {
            ready_groups += 1;
        }
    }

    SweepEvidence {
        total_groups,
        ready_groups,
    }
}

async fn observe_peer(
    network: Network,
    addr: PeerSocketAddr,
    user_agent: String,
    target: ActivationTarget,
) -> bool {
    let handshake = zebra_network::connect_isolated_tcp_direct(&network, addr, user_agent);
    let Ok(Ok(client)) = tokio::time::timeout(OBSERVATION_TIMEOUT, handshake).await else {
        return false;
    };
    let remote = &client.connection_info.remote;

    remote.start_height >= target.confirmation_height
        && remote.version >= target.required_version
        && remote.services.contains(PeerServices::NODE_NETWORK)
}

async fn persist_confirmation(path: Option<PathBuf>, target: ActivationTarget) -> io::Result<()> {
    let path = path.ok_or_else(|| {
        io::Error::other("zebra-network cache is disabled; no durable confirmation path")
    })?;
    let record = target.confirmation_record();

    tokio::task::spawn_blocking(move || {
        atomic_write(path, record.as_bytes())?
            .map(|_| ())
            .map_err(|error| error.error)
    })
    .await
    .map_err(|error| io::Error::other(format!("activation persistence task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
    };

    use super::*;
    use zebra_chain::{block::Height, parameters::Network};
    use zebra_network::Version;

    #[test]
    fn half_ready_never_confirms() {
        let mut gate = ActivationGate::default();

        for _ in 0..10 {
            assert!(!gate.observe(SweepEvidence {
                total_groups: 12,
                ready_groups: 6,
            }));
        }
    }

    #[test]
    fn fixed_quorum_confirms_on_the_third_consecutive_sweep() {
        let mut gate = ActivationGate::default();
        let quorum = SweepEvidence {
            total_groups: 12,
            ready_groups: 9,
        };

        assert!(!gate.observe(quorum));
        assert!(!gate.observe(quorum));
        assert!(gate.observe(quorum));
    }

    #[test]
    fn non_qualifying_sweep_resets_confirmation_progress() {
        let mut gate = ActivationGate::default();
        let quorum = SweepEvidence {
            total_groups: 12,
            ready_groups: 9,
        };

        assert!(!gate.observe(quorum));
        assert!(!gate.observe(quorum));
        assert!(!gate.observe(SweepEvidence {
            total_groups: 12,
            ready_groups: 8,
        }));
        assert!(!gate.observe(quorum));
        assert!(!gate.observe(quorum));
        assert!(gate.observe(quorum));
    }

    #[test]
    fn quorum_below_the_diversity_floor_never_confirms() {
        let mut gate = ActivationGate::default();
        let small_quorum = SweepEvidence {
            total_groups: 8,
            ready_groups: 8,
        };

        for _ in 0..10 {
            assert!(!gate.observe(small_quorum));
        }
    }

    #[test]
    fn network_groups_use_ipv4_16_and_ipv6_32_prefixes() {
        assert_eq!(
            network_group(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))),
            network_group(IpAddr::V4(Ipv4Addr::new(1, 2, 200, 201)))
        );
        assert_ne!(
            network_group(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))),
            network_group(IpAddr::V4(Ipv4Addr::new(1, 3, 3, 4)))
        );
        assert_eq!(
            network_group(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 3, 4, 5, 6))),
            network_group(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 9, 8, 7, 6, 5, 4)))
        );
        assert_ne!(
            network_group(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 3, 4, 5, 6))),
            network_group(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb9, 1, 2, 3, 4, 5, 6)))
        );
    }

    #[test]
    fn latest_mainnet_target_requires_reorg_safe_nu6_3_depth() {
        let target = ActivationTarget::latest(&Network::Mainnet);

        assert_eq!(target.activation_height, Height(3_428_143));
        assert_eq!(target.confirmation_height, Height(3_429_143));
        assert_eq!(target.required_version, Version(170_160));
        assert_eq!(target.pre_activation_height, Height(3_428_142));
    }

    #[test]
    fn latest_testnet_target_requires_reorg_safe_nu6_3_depth() {
        let target = ActivationTarget::latest(&Network::new_default_testnet());

        assert_eq!(target.activation_height, Height(4_134_000));
        assert_eq!(target.confirmation_height, Height(4_135_000));
        assert_eq!(target.required_version, Version(170_160));
        assert_eq!(target.pre_activation_height, Height(4_133_999));
    }

    #[test]
    fn persisted_confirmation_must_match_the_compiled_target_exactly() {
        let target = ActivationTarget::latest(&Network::Mainnet);
        let record = target.confirmation_record();

        assert!(target.matches_confirmation_record(&record));
        assert!(!target.matches_confirmation_record(&record.replace(
            "minimum_protocol_version=170160",
            "minimum_protocol_version=170150"
        )));
    }

    #[tokio::test]
    async fn durable_confirmation_round_trips() -> Result<(), Box<dyn Error>> {
        let target = ActivationTarget::latest(&Network::Mainnet);
        let path = std::env::temp_dir().join(format!(
            "zeeder-activation-{}-{}.record",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));

        persist_confirmation(Some(path.clone()), target).await?;
        assert!(load_confirmation(Some(&path), target).await);
        std::fs::remove_file(path)?;

        Ok(())
    }
}
