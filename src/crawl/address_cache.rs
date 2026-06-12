use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use metrics::{counter, gauge};
use rand::{rng, seq::SliceRandom};
use tokio::sync::watch;
use zebra_chain::parameters::Network;
use zebra_network::{AddressBook, PeerSocketAddr};

use crate::{
    crawl::servability::{UnservableReason, classify_peer},
    metrics::{
        ADDR_FAMILY_IPV4, ADDR_FAMILY_IPV6, LABEL_ADDR_FAMILY, LABEL_REASON, MUTEX_POISONING_TOTAL,
        PEERS_KNOWN, PEERS_SERVABLE, PEERS_UNSERVABLE,
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

/// Spawns the background task that refreshes the served-address cache.
pub(crate) fn spawn(
    address_book: Arc<std::sync::Mutex<AddressBook>>,
    network: Network,
) -> watch::Receiver<ServablePeers> {
    let (servable_peers_sender, servable_peers_receiver) = watch::channel(ServablePeers::default());

    tokio::spawn(async move {
        let mut refresh_count = 0u64;

        loop {
            tokio::time::sleep(CACHE_REFRESH_INTERVAL).await;
            refresh_count = refresh_count.wrapping_add(1);
            let should_log_status = refresh_count.is_multiple_of(CRAWLER_STATUS_LOG_REFRESHES);

            let servable_peers = {
                let guard = match address_book.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        tracing::error!(
                            "address book mutex poisoned during cache update, recovering"
                        );
                        counter!(MUTEX_POISONING_TOTAL).increment(1);
                        poisoned.into_inner()
                    }
                };
                servable_peers(&guard, &network, should_log_status)
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
/// Shuffling the full servable set (rather than a fixed-size prefix) before
/// truncating gives every servable peer an equal chance of being served, which
/// matters for even load distribution and sybil resistance.
#[allow(
    clippy::cast_precision_loss,
    reason = "gauge values are peer counts; f64 precision loss is irrelevant"
)]
fn servable_peers(book: &AddressBook, network: &Network, should_log_status: bool) -> ServablePeers {
    let now = Utc::now();

    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    let mut unservable: HashMap<UnservableReason, usize> = HashMap::new();

    for meta in book.peers() {
        match classify_peer(&meta, now, network) {
            Ok(()) => {
                let addr = meta.addr();
                if addr.ip().is_ipv4() {
                    ipv4.push(addr);
                } else {
                    ipv6.push(addr);
                }
            }
            Err(reason) => *unservable.entry(reason).or_default() += 1,
        }
    }

    gauge!(PEERS_KNOWN).set(book.len() as f64);
    gauge!(PEERS_SERVABLE, LABEL_ADDR_FAMILY => ADDR_FAMILY_IPV4).set(ipv4.len() as f64);
    gauge!(PEERS_SERVABLE, LABEL_ADDR_FAMILY => ADDR_FAMILY_IPV6).set(ipv6.len() as f64);
    for reason in UnservableReason::ALL {
        gauge!(PEERS_UNSERVABLE, LABEL_REASON => reason.label())
            .set(unservable.get(&reason).copied().unwrap_or(0) as f64);
    }

    if should_log_status {
        tracing::info!(
            total = book.len(),
            servable_v4 = ipv4.len(),
            servable_v6 = ipv6.len(),
            "crawler status"
        );
    }

    let mut rng = rng();
    ipv4.shuffle(&mut rng);
    ipv4.truncate(MAX_DNS_RESPONSE_PEERS);
    ipv6.shuffle(&mut rng);
    ipv6.truncate(MAX_DNS_RESPONSE_PEERS);

    ServablePeers {
        ipv4: ipv4.into(),
        ipv6: ipv6.into(),
    }
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
        book.update(MetaAddr::new_connected(
            addr,
            &services,
            is_inbound,
            TEST_USER_AGENT.to_string(),
            CURRENT_NETWORK_PROTOCOL_VERSION,
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

        let peers = servable_peers(&book, &Network::Mainnet, false);
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

        let peers = servable_peers(&book, &Network::Mainnet, false);
        let served: Vec<IpAddr> = peers.ipv4.iter().map(|p| p.ip()).collect();
        assert_eq!(served, vec![IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))]);
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

        let peers = servable_peers(&book, &Network::Mainnet, false);
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

        let peers = servable_peers(&book, &Network::Mainnet, false);
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

        let peers = servable_peers(&book, &Network::Mainnet, false);
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

        let peers = servable_peers(&book, &Network::Mainnet, false);
        assert!(
            peers.ipv4.is_empty(),
            "peers on a non-default port must not be served"
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
