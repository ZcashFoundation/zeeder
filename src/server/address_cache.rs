use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use metrics::{counter, gauge};
use rand::{rng, seq::SliceRandom};
use tokio::sync::watch;
use zebra_chain::parameters::Network;
use zebra_network::{AddressBook, PeerSocketAddr};

use crate::server::eligibility::{IneligibleReason, classify_peer};

/// Maximum addresses returned per DNS query, per address family.
const MAX_DNS_RESPONSE_PEERS: usize = 25;

/// How often the served-address cache is recomputed from the address book.
const CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Cached address records for lock-free DNS response generation.
///
/// Updated periodically by a background task so DNS queries read a shuffled,
/// pre-filtered snapshot without ever locking the address book.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddressRecords {
    pub(crate) ipv4: Vec<PeerSocketAddr>,
    pub(crate) ipv6: Vec<PeerSocketAddr>,
}

/// Spawns the background task that refreshes the served-address cache.
pub(crate) fn spawn(
    address_book: Arc<std::sync::Mutex<AddressBook>>,
    network: Network,
) -> watch::Receiver<AddressRecords> {
    let (latest_addresses_sender, latest_addresses) = watch::channel(AddressRecords::default());

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CACHE_REFRESH_INTERVAL).await;

            let records = {
                let guard = match address_book.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        tracing::error!(
                            "address book mutex poisoned during cache update, recovering"
                        );
                        counter!("seeder_mutex_poisoning_total", "location" => "cache_updater")
                            .increment(1);
                        poisoned.into_inner()
                    }
                };
                servable_records(&guard, &network)
            };

            let _ = latest_addresses_sender.send(records);
        }
    });

    latest_addresses
}

/// Classify every peer in the book, publish servable and per-reason ineligible
/// counts as gauges, and return a shuffled, capped set of servable addresses.
///
/// Shuffling the full servable set (rather than a fixed-size prefix) before
/// truncating gives every servable peer an equal chance of being served, which
/// matters for even load distribution and sybil resistance.
#[allow(
    clippy::cast_precision_loss,
    reason = "gauge values are peer counts; f64 precision loss is irrelevant"
)]
fn servable_records(book: &AddressBook, network: &Network) -> AddressRecords {
    let now = Utc::now();
    let banned: HashSet<IpAddr> = book.bans().keys().copied().collect();

    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    let mut ineligible: HashMap<IneligibleReason, usize> = HashMap::new();

    for meta in book.peers() {
        match classify_peer(&meta, now, &banned, network) {
            Ok(()) => {
                let addr = meta.addr();
                if addr.ip().is_ipv4() {
                    ipv4.push(addr);
                } else {
                    ipv6.push(addr);
                }
            }
            Err(reason) => *ineligible.entry(reason).or_default() += 1,
        }
    }

    gauge!("seeder_peers_known").set(book.len() as f64);
    gauge!("seeder_peers_servable", "addr_family" => "v4").set(ipv4.len() as f64);
    gauge!("seeder_peers_servable", "addr_family" => "v6").set(ipv6.len() as f64);
    for reason in IneligibleReason::ALL {
        gauge!("seeder_peers_ineligible", "reason" => reason.label())
            .set(ineligible.get(&reason).copied().unwrap_or(0) as f64);
    }

    let mut rng = rng();
    ipv4.shuffle(&mut rng);
    ipv4.truncate(MAX_DNS_RESPONSE_PEERS);
    ipv6.shuffle(&mut rng);
    ipv6.truncate(MAX_DNS_RESPONSE_PEERS);

    AddressRecords { ipv4, ipv6 }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use tracing::Span;
    use zebra_chain::parameters::Network;
    use zebra_network::types::{MetaAddr, PeerServices};

    use super::*;

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

    /// Never-handshaked peers are in the book but must never be served.
    #[test]
    fn never_handshaked_peers_are_not_servable() {
        let mut book = empty_book();
        book.update(MetaAddr::new_initial_peer(peer([1, 2, 3, 4], 8233)));
        assert_eq!(book.len(), 1, "the peer should be in the book");

        let records = servable_records(&book, &Network::Mainnet);
        assert!(
            records.ipv4.is_empty() && records.ipv6.is_empty(),
            "never-handshaked peers must not be served"
        );
    }

    /// A recently-handshaked full node (advertising NODE_NETWORK) is servable.
    #[test]
    fn recently_connected_full_node_is_servable() {
        let mut book = empty_book();
        book.update(MetaAddr::new_connected(
            peer([1, 2, 3, 4], 8233),
            &PeerServices::NODE_NETWORK,
            false,
        ));

        let records = servable_records(&book, &Network::Mainnet);
        let served: Vec<IpAddr> = records.ipv4.iter().map(|p| p.ip()).collect();
        assert_eq!(served, vec![IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))]);
    }

    /// A recently-handshaked peer that does not advertise NODE_NETWORK is not
    /// servable: zebra's handshake enforces the version floor but not the
    /// full-node service, so the seeder must gate on it.
    #[test]
    fn recently_connected_non_full_node_is_not_servable() {
        let mut book = empty_book();
        book.update(MetaAddr::new_connected(
            peer([1, 2, 3, 4], 8233),
            &PeerServices::empty(),
            false,
        ));

        let records = servable_records(&book, &Network::Mainnet);
        assert!(
            records.ipv4.is_empty() && records.ipv6.is_empty(),
            "a recently-live non-full-node peer must not be served"
        );
    }

    /// A handshaked peer on a non-default port cannot be reached via DNS (which
    /// carries no port), so it is not servable.
    #[test]
    fn responded_peer_on_wrong_port_is_not_servable() {
        let mut book = empty_book();
        book.update(MetaAddr::new_connected(
            peer([1, 2, 3, 4], 1234),
            &PeerServices::NODE_NETWORK,
            false,
        ));

        let records = servable_records(&book, &Network::Mainnet);
        assert!(
            records.ipv4.is_empty(),
            "peers on a non-default port must not be served"
        );
    }
}
