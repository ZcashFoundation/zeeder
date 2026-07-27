use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use metrics::{counter, gauge};
use rand::{rng, seq::SliceRandom};
use tokio::sync::watch;
use zebra_chain::{chain_tip::ChainTip, parameters::Network};
use zebra_network::{AddressBook, PeerSocketAddr, Version};

use crate::{
    config::ZcashNetwork,
    crawl::{
        chain_tip::SeederChainTip,
        servability::{UnservableReason, classify_peer},
    },
    metrics::{
        ADDR_FAMILY_IPV4, ADDR_FAMILY_IPV6, LABEL_ADDR_FAMILY, LABEL_NETWORK, LABEL_REASON,
        MUTEX_POISONING_TOTAL, PEERS_KNOWN, PEERS_SERVABLE, PEERS_UNSERVABLE,
    },
};

/// Maximum addresses returned per DNS query, per address family.
const MAX_DNS_RESPONSE_PEERS: usize = 25;

/// How often the served-address cache is recomputed from the address book.
const CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// How many cache refreshes happen between crawler status logs.
const CRAWLER_STATUS_LOG_REFRESHES: u64 = 120;

/// Cached servable peers for lock-free DNS response generation.
///
/// Updated periodically by a background task so DNS queries read a shuffled,
/// pre-filtered snapshot without ever locking the address book. Address-family
/// slices are reference-counted so DNS queries can clone the snapshot without
/// copying the peer lists.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServablePeers {
    pub(crate) ipv4: Arc<[PeerSocketAddr]>,
    pub(crate) ipv6: Arc<[PeerSocketAddr]>,
}

impl ServablePeers {
    /// Total servable peers across both address families.
    pub(crate) fn total(&self) -> usize {
        self.ipv4.len() + self.ipv6.len()
    }
}

/// Spawns the background task that refreshes one network's served-address cache.
pub(crate) fn spawn(
    address_book: Arc<std::sync::Mutex<AddressBook>>,
    network: ZcashNetwork,
    tip: SeederChainTip,
    target_version: Version,
) -> watch::Receiver<ServablePeers> {
    let (servable_peers_sender, servable_peers_receiver) = watch::channel(ServablePeers::default());

    let zcash_network = network.to_zebra();
    let network_label = network.label();

    tokio::spawn(async move {
        let mut refresh_count = 0u64;

        loop {
            tokio::time::sleep(CACHE_REFRESH_INTERVAL).await;
            refresh_count = refresh_count.wrapping_add(1);
            let should_log_status = refresh_count.is_multiple_of(CRAWLER_STATUS_LOG_REFRESHES);
            let minimum_version =
                Version::min_remote_for_height(&zcash_network, tip.best_tip_height());

            let servable_peers = {
                let guard = match address_book.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        tracing::error!(
                            network = network_label,
                            "address book mutex poisoned during cache update, recovering"
                        );
                        counter!(MUTEX_POISONING_TOTAL, LABEL_NETWORK => network_label)
                            .increment(1);
                        poisoned.into_inner()
                    }
                };
                servable_peers(
                    &guard,
                    &zcash_network,
                    minimum_version,
                    target_version,
                    network_label,
                    should_log_status,
                )
            };

            if servable_peers_sender.send(servable_peers).is_err() {
                tracing::debug!("servable peer cache receiver dropped, stopping cache updater");
                break;
            }
        }
    });

    servable_peers_receiver
}

/// Classify every peer in the book, publish servable and per-reason unservable
/// counts as gauges, and return a shuffled, capped set of servable addresses.
///
/// Peers at the compiled target version are served before peers at the previous
/// admitted floor. Each tier is shuffled before truncation so peers within the
/// same tier rotate evenly.
#[allow(
    clippy::cast_precision_loss,
    reason = "gauge values are peer counts; f64 precision loss is irrelevant"
)]
fn servable_peers(
    book: &AddressBook,
    network: &Network,
    minimum_version: Version,
    target_version: Version,
    network_label: &'static str,
    should_log_status: bool,
) -> ServablePeers {
    let now = Utc::now();

    let mut target_ipv4 = Vec::new();
    let mut fallback_ipv4 = Vec::new();
    let mut target_ipv6 = Vec::new();
    let mut fallback_ipv6 = Vec::new();
    let mut unservable: HashMap<UnservableReason, usize> = HashMap::new();

    for meta in book.peers() {
        match classify_peer(&meta, now, network, minimum_version) {
            Ok(()) => {
                let addr = meta.addr();
                let is_target_version = meta
                    .negotiated_version()
                    .is_some_and(|version| version >= target_version);

                match (addr.ip().is_ipv4(), is_target_version) {
                    (true, true) => target_ipv4.push(addr),
                    (true, false) => fallback_ipv4.push(addr),
                    (false, true) => target_ipv6.push(addr),
                    (false, false) => fallback_ipv6.push(addr),
                }
            }
            Err(reason) => *unservable.entry(reason).or_default() += 1,
        }
    }

    let servable_ipv4 = target_ipv4.len() + fallback_ipv4.len();
    let servable_ipv6 = target_ipv6.len() + fallback_ipv6.len();

    gauge!(PEERS_KNOWN, LABEL_NETWORK => network_label).set(book.len() as f64);
    gauge!(PEERS_SERVABLE, LABEL_NETWORK => network_label, LABEL_ADDR_FAMILY => ADDR_FAMILY_IPV4)
        .set(servable_ipv4 as f64);
    gauge!(PEERS_SERVABLE, LABEL_NETWORK => network_label, LABEL_ADDR_FAMILY => ADDR_FAMILY_IPV6)
        .set(servable_ipv6 as f64);
    for reason in UnservableReason::ALL {
        gauge!(PEERS_UNSERVABLE, LABEL_NETWORK => network_label, LABEL_REASON => reason.label())
            .set(unservable.get(&reason).copied().unwrap_or(0) as f64);
    }

    if should_log_status {
        tracing::info!(
            network = network_label,
            total = book.len(),
            servable_v4 = servable_ipv4,
            servable_v6 = servable_ipv6,
            target_version_v4 = target_ipv4.len(),
            target_version_v6 = target_ipv6.len(),
            "crawler status"
        );
    }

    ServablePeers {
        ipv4: shuffled_version_preferred_peers(target_ipv4, fallback_ipv4).into(),
        ipv6: shuffled_version_preferred_peers(target_ipv6, fallback_ipv6).into(),
    }
}

fn shuffled_version_preferred_peers(
    mut target_version: Vec<PeerSocketAddr>,
    mut fallback_version: Vec<PeerSocketAddr>,
) -> Vec<PeerSocketAddr> {
    let mut rng = rng();
    target_version.shuffle(&mut rng);
    fallback_version.shuffle(&mut rng);
    target_version.append(&mut fallback_version);
    target_version.truncate(MAX_DNS_RESPONSE_PEERS);
    target_version
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use tracing::Span;
    use zebra_chain::parameters::Network;
    use zebra_network::constants::{CURRENT_NETWORK_PROTOCOL_VERSION, MAX_PEER_MISBEHAVIOR_SCORE};
    use zebra_network::types::{MetaAddr, PeerServices};

    use super::*;

    const TEST_USER_AGENT: &str = "/zeeder-test/";

    fn empty_book() -> AddressBook {
        AddressBook::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8233),
            &Network::Mainnet,
            100,
            Span::none(),
        )
    }

    fn peer(octets: [u8; 4], port: u16) -> PeerSocketAddr {
        PeerSocketAddr::from(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
    }

    fn update_connected_peer(
        book: &mut AddressBook,
        addr: PeerSocketAddr,
        services: PeerServices,
        is_inbound: bool,
    ) {
        update_connected_peer_with_version(
            book,
            addr,
            services,
            is_inbound,
            CURRENT_NETWORK_PROTOCOL_VERSION,
        );
    }

    fn update_connected_peer_with_version(
        book: &mut AddressBook,
        addr: PeerSocketAddr,
        services: PeerServices,
        is_inbound: bool,
        version: Version,
    ) {
        book.update(MetaAddr::new_connected(
            addr,
            &services,
            is_inbound,
            TEST_USER_AGENT.to_string(),
            version,
        ));
    }

    #[test]
    fn servable_peer_snapshots_clone_without_copying_peer_lists() {
        let peers = ServablePeers {
            ipv4: vec![peer([1, 2, 3, 4], 8233)].into(),
            ipv6: Arc::default(),
        };

        let cloned_peers = peers.clone();

        assert!(Arc::ptr_eq(&peers.ipv4, &cloned_peers.ipv4));
        assert!(Arc::ptr_eq(&peers.ipv6, &cloned_peers.ipv6));
    }

    /// Never-handshaked peers are in the book but must never be served.
    #[test]
    fn never_handshaked_peers_are_not_servable() {
        let mut book = empty_book();
        book.update(MetaAddr::new_initial_peer(peer([1, 2, 3, 4], 8233)));
        assert_eq!(book.len(), 1, "the peer should be in the book");

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            "mainnet",
            false,
        );
        assert!(
            peers.ipv4.is_empty() && peers.ipv6.is_empty(),
            "never-handshaked peers must not be served"
        );
    }

    /// A recently-handshaked full node (advertising NODE_NETWORK) is servable.
    #[test]
    fn recently_connected_full_node_is_servable() {
        let mut book = empty_book();
        update_connected_peer(
            &mut book,
            peer([1, 2, 3, 4], 8233),
            PeerServices::NODE_NETWORK,
            false,
        );

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            "mainnet",
            false,
        );
        let served: Vec<IpAddr> = peers.ipv4.iter().map(|p| p.ip()).collect();
        assert_eq!(served, vec![IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))]);
    }

    #[test]
    fn peer_admitted_before_activation_is_removed_by_the_new_floor() {
        let mut book = empty_book();
        book.update(MetaAddr::new_connected(
            peer([1, 2, 3, 4], 8233),
            &PeerServices::NODE_NETWORK,
            false,
            TEST_USER_AGENT.to_string(),
            zebra_network::Version(170_150),
        ));

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            zebra_network::Version(170_160),
            zebra_network::Version(170_160),
            "mainnet",
            false,
        );

        assert!(
            peers.ipv4.is_empty() && peers.ipv6.is_empty(),
            "a cached handshake below the raised floor must stop being served"
        );
    }

    #[test]
    fn recently_connected_non_full_node_is_not_servable() {
        let mut book = empty_book();
        update_connected_peer(
            &mut book,
            peer([1, 2, 3, 4], 8233),
            PeerServices::empty(),
            false,
        );

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            "mainnet",
            false,
        );
        assert!(
            peers.ipv4.is_empty() && peers.ipv6.is_empty(),
            "a recently-live non-full-node peer must not be served"
        );
    }

    #[test]
    fn recently_connected_inbound_peer_is_not_servable() {
        let mut book = empty_book();
        update_connected_peer(
            &mut book,
            peer([1, 2, 3, 4], 8233),
            PeerServices::NODE_NETWORK,
            true,
        );

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            "mainnet",
            false,
        );
        assert!(
            peers.ipv4.is_empty() && peers.ipv6.is_empty(),
            "an inbound peer must not be served"
        );
    }

    #[test]
    fn sub_ban_misbehaving_peer_is_not_servable() {
        let mut book = empty_book();
        let addr = peer([1, 2, 3, 4], 8233);
        let misbehavior_score = MAX_PEER_MISBEHAVIOR_SCORE - 1;
        update_connected_peer(&mut book, addr, PeerServices::NODE_NETWORK, false);
        book.update(MetaAddr::new_misbehavior(addr, misbehavior_score));

        assert_eq!(
            book.len(),
            1,
            "a sub-ban misbehaving peer remains in the address book"
        );
        assert!(
            book.peers()
                .any(|meta| meta.misbehavior() == misbehavior_score),
            "the peer should carry the sub-ban misbehavior score"
        );

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            "mainnet",
            false,
        );
        assert!(
            peers.ipv4.is_empty() && peers.ipv6.is_empty(),
            "a misbehaving peer must not be served"
        );
    }

    /// A handshaked peer on a non-default port cannot be reached via DNS (which
    /// carries no port), so it is not servable.
    #[test]
    fn responded_peer_on_wrong_port_is_not_servable() {
        let mut book = empty_book();
        update_connected_peer(
            &mut book,
            peer([1, 2, 3, 4], 1234),
            PeerServices::NODE_NETWORK,
            false,
        );

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            CURRENT_NETWORK_PROTOCOL_VERSION,
            "mainnet",
            false,
        );
        assert!(
            peers.ipv4.is_empty(),
            "peers on a non-default port must not be served"
        );
    }

    #[test]
    fn target_version_peers_fill_dns_responses_before_fallback_peers() {
        let mut book = empty_book();
        let previous_version = Version(170_150);
        let target_version = Version(170_160);

        for host in 1..=30 {
            update_connected_peer_with_version(
                &mut book,
                peer([20, 1, 1, host], 8233),
                PeerServices::NODE_NETWORK,
                false,
                target_version,
            );
            update_connected_peer_with_version(
                &mut book,
                peer([30, 1, 1, host], 8233),
                PeerServices::NODE_NETWORK,
                false,
                previous_version,
            );
        }

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            previous_version,
            target_version,
            "mainnet",
            false,
        );

        assert_eq!(peers.ipv4.len(), MAX_DNS_RESPONSE_PEERS);
        assert!(
            peers.ipv4.iter().all(|addr| match addr.ip() {
                IpAddr::V4(ip) => ip.octets()[0] == 20,
                IpAddr::V6(_) => false,
            }),
            "a full target-version tier must exclude fallback peers from the response"
        );
    }

    #[test]
    fn fallback_peers_top_up_a_sparse_target_version_tier() {
        let mut book = empty_book();
        let previous_version = Version(170_150);
        let target_version = Version(170_160);

        for host in 1..=5 {
            update_connected_peer_with_version(
                &mut book,
                peer([20, 1, 1, host], 8233),
                PeerServices::NODE_NETWORK,
                false,
                target_version,
            );
        }
        for host in 1..=25 {
            update_connected_peer_with_version(
                &mut book,
                peer([30, 1, 1, host], 8233),
                PeerServices::NODE_NETWORK,
                false,
                previous_version,
            );
        }

        let peers = servable_peers(
            &book,
            &Network::Mainnet,
            previous_version,
            target_version,
            "mainnet",
            false,
        );
        let target_version_count = peers
            .ipv4
            .iter()
            .filter(|addr| match addr.ip() {
                IpAddr::V4(ip) => ip.octets()[0] == 20,
                IpAddr::V6(_) => false,
            })
            .count();

        assert_eq!(peers.ipv4.len(), MAX_DNS_RESPONSE_PEERS);
        assert_eq!(
            target_version_count, 5,
            "all available target-version peers must be retained before fallback top-up"
        );
    }

    /// zebra-network removes a peer from the book when it bans it, so the seeder
    /// never has to filter banned IPs itself.
    #[test]
    fn banned_peers_are_removed_from_the_book() {
        let mut book = empty_book();
        let addr = peer([1, 2, 3, 4], 8233);
        update_connected_peer(&mut book, addr, PeerServices::NODE_NETWORK, false);
        assert_eq!(book.len(), 1, "the peer starts in the book");

        book.update(MetaAddr::new_misbehavior(addr, MAX_PEER_MISBEHAVIOR_SCORE));

        assert_eq!(book.len(), 0, "a banned peer is removed from the book");
        assert!(
            book.bans().contains_key(&addr.ip()),
            "the banned ip is recorded in the ban set"
        );
    }
}
