use std::{sync::Arc, time::Duration};

use rand::{rng, seq::SliceRandom};
use tokio::sync::watch;

use zebra_network::PeerSocketAddr;

// DNS response configuration
const MAX_DNS_RESPONSE_PEERS: usize = 25;
const PEER_SELECTION_POOL_SIZE: usize = 50; // Collect 2x peers for shuffle randomness

/// Cached address records for lock-free DNS response generation.
/// Updated periodically by a background task to avoid lock contention.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct AddressRecords {
    pub ipv4: Vec<PeerSocketAddr>,
    pub ipv6: Vec<PeerSocketAddr>,
}

/// Spawns a background task that periodically updates the cached address records.
/// This eliminates lock contention during DNS query handling by providing a
/// lock-free read path via the watch channel.
pub fn spawn(
    address_book: Arc<std::sync::Mutex<zebra_network::AddressBook>>,
    default_port: u16,
) -> watch::Receiver<AddressRecords> {
    let (latest_addresses_sender, latest_addresses) = watch::channel(AddressRecords::default());

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;

            let matched_peers: Vec<_> = match address_book.lock() {
                Ok(guard) => guard
                    .peers()
                    .filter(|meta| {
                        let ip = meta.addr().ip();

                        // 1. Routability check
                        let is_global =
                            !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast();

                        // 2. Port check
                        let is_default_port = meta.addr().port() == default_port;

                        is_global && is_default_port
                    })
                    .collect::<Vec<_>>(),
                Err(poisoned) => {
                    tracing::error!("Address book mutex poisoned during cache update, recovering");
                    metrics::counter!("seeder.mutex_poisoning_total", "location" => "cache_updater")
                        .increment(1);
                    poisoned
                        .into_inner()
                        .peers()
                        .filter(|meta| {
                            let ip = meta.addr().ip();
                            let is_global =
                                !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast();
                            let is_default_port = meta.addr().port() == default_port;
                            is_global && is_default_port
                        })
                        .collect::<Vec<_>>()
                }
            };

            // Separate into IPv4 and IPv6 pools
            let mut ipv4: Vec<_> = matched_peers
                .iter()
                .filter(|meta| meta.addr().ip().is_ipv4())
                .take(PEER_SELECTION_POOL_SIZE)
                .collect();
            let mut ipv6: Vec<_> = matched_peers
                .iter()
                .filter(|meta| meta.addr().ip().is_ipv6())
                .take(PEER_SELECTION_POOL_SIZE)
                .collect();

            // Shuffle and take the configured maximum
            ipv4.shuffle(&mut rng());
            ipv4.truncate(MAX_DNS_RESPONSE_PEERS);
            ipv6.shuffle(&mut rng());
            ipv6.truncate(MAX_DNS_RESPONSE_PEERS);

            let ipv4 = ipv4.iter().map(|peer| peer.addr()).collect::<Vec<_>>();
            let ipv6 = ipv6.iter().map(|peer| peer.addr()).collect::<Vec<_>>();

            let _ = latest_addresses_sender.send(AddressRecords { ipv4, ipv6 });
        }
    });

    latest_addresses
}

// DNS Response Constants Tests
#[test]
fn test_dns_response_constants() {
    // Verify the constants are reasonable
    assert_eq!(
        MAX_DNS_RESPONSE_PEERS, 25,
        "Should return max 25 peers per query"
    );
    assert_eq!(
        PEER_SELECTION_POOL_SIZE, 50,
        "Should collect 50 peers for randomization"
    );
    assert!(
        PEER_SELECTION_POOL_SIZE >= MAX_DNS_RESPONSE_PEERS * 2,
        "Pool should be at least 2x response size for good randomization"
    );
}
