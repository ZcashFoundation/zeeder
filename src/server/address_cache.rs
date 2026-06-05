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
use zebra_network::{AddressBook, PeerSocketAddr};

use crate::server::eligibility::{classify_peer, IneligibleReason};

/// Maximum addresses returned per DNS query, per address family.
const MAX_DNS_RESPONSE_PEERS: usize = 25;

/// How often the served-address cache is recomputed from the address book.
const CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Cached address records for lock-free DNS response generation.
///
/// Updated periodically by a background task so DNS queries read a shuffled,
/// pre-filtered snapshot without ever locking the address book.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct AddressRecords {
    pub ipv4: Vec<PeerSocketAddr>,
    pub ipv6: Vec<PeerSocketAddr>,
}

/// Spawns the background task that refreshes the served-address cache.
pub fn spawn(
    address_book: Arc<std::sync::Mutex<AddressBook>>,
    default_port: u16,
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
                servable_records(&guard, default_port)
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
fn servable_records(book: &AddressBook, default_port: u16) -> AddressRecords {
    let now = Utc::now();
    let banned: HashSet<IpAddr> = book.bans().keys().copied().collect();

    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    let mut ineligible: HashMap<IneligibleReason, usize> = HashMap::new();

    for meta in book.peers() {
        match classify_peer(&meta, now, default_port, &banned) {
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
