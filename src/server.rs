use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};

use color_eyre::eyre::{Context, Result};
use hickory_proto::{
    op::{Header, HeaderCounts, Metadata, ResponseCode},
    rr::{RData, Record, RecordType},
};
use hickory_server::{
    net::runtime::Time,
    server::{RequestHandler, ResponseHandler, ResponseInfo, Server},
    zone_handler::MessageResponseBuilder,
};
use metrics::{counter, gauge, histogram};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::watch,
    time,
};
use tracing::{Instrument, info_span};
use zebra_chain::{chain_tip::ChainTip, parameters::Network};

use crate::{
    config::SeederConfig,
    server::{
        address_cache::AddressRecords,
        rate_limiter::{RateLimiter, RateLimiterExt},
    },
};

mod address_cache;
mod chain_tip;
mod eligibility;
mod rate_limiter;

/// How often the metrics logger task logs address-book status. zebra-network
/// crawls continuously; this interval only controls periodic status logging.
#[allow(
    clippy::duration_suboptimal_units,
    reason = "seconds keeps this interval consistent with the other Duration constants"
)]
const METRICS_LOG_INTERVAL: Duration = Duration::from_secs(600);

pub(crate) async fn spawn(config: SeederConfig) -> Result<()> {
    tracing::info!("Initializing zebra-network...");

    // Dummy inbound service that rejects everything
    let inbound_service = tower::service_fn(|_req: zebra_network::Request| async move {
        Ok::<zebra_network::Response, Box<dyn std::error::Error + Send + Sync + 'static>>(
            zebra_network::Response::Nil,
        )
    });

    // Provide a user agent
    let user_agent = option_env!("VERGEN_GIT_SHA").map_or_else(
        || format!("zebra-seeder/{}", env!("CARGO_PKG_VERSION")),
        |sha| {
            let short_sha = &sha[..7.min(sha.len())];
            format!("zebra-seeder/{} ({short_sha})", env!("CARGO_PKG_VERSION"))
        },
    );

    tracing::info!("User-Agent: {user_agent}");

    // Pin a chain tip at the current network upgrade so zebra-network's
    // handshake rejects peers advertising an outdated protocol version.
    let tip = chain_tip::SeederChainTip::current_upgrade(&config.network.network);
    let min_protocol_version = zebra_network::Version::min_remote_for_height(
        &config.network.network,
        tip.best_tip_height(),
    );
    tracing::info!(
        network = %config.network.network,
        %min_protocol_version,
        "enforcing peer protocol-version floor"
    );
    gauge!("seeder_min_protocol_version").set(f64::from(min_protocol_version.0));
    gauge!(
        "seeder_build_info",
        "version" => env!("CARGO_PKG_VERSION"),
        "network" => config.network.network.to_string(),
    )
    .set(1.0);

    // Initialize zebra-network
    let (peer_set, address_book, _peer_sender) =
        zebra_network::init(config.network.clone(), inbound_service, tip, user_agent).await;

    // Spawn the metrics logger task
    let address_book_monitor = address_book.clone();
    let network = config.network.network.clone();

    let metrics_handle = tokio::spawn(async move {
        // Keep peer_set alive to ensure the network stack keeps running
        let _keep_alive = peer_set;

        let mut interval = time::interval(METRICS_LOG_INTERVAL);

        loop {
            interval.tick().await;

            // Log Address Book stats
            let book = match address_book_monitor.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    tracing::error!(
                        "Address book mutex poisoned during metrics logging, recovering"
                    );
                    counter!("seeder_mutex_poisoning_total", "location" => "metrics_logger")
                        .increment(1);
                    poisoned.into_inner()
                }
            };
            log_crawler_status(&book, &network);
        }
    });

    // Initialize rate limiter if configured
    let (rate_limiter, prune_task) = rate_limiter::spawn(&config);

    tracing::info!("Initializing DNS server on {}", config.dns_listen_addr);

    // Spawn address cache updater - provides lock-free reads for DNS queries
    let latest_addresses =
        address_cache::spawn(address_book.clone(), config.network.network.clone());

    let authority = SeederAuthority::new(
        latest_addresses,
        config.seed_domain.clone(),
        config.dns_ttl,
        rate_limiter,
    );
    let mut server = Server::new(authority);

    // Register UDP and TCP listeners
    let udp_socket = UdpSocket::bind(config.dns_listen_addr)
        .await
        .wrap_err("failed to bind UDP socket")?;
    server.register_socket(udp_socket);

    let tcp_listener = TcpListener::bind(config.dns_listen_addr)
        .await
        .wrap_err("failed to bind TCP listener")?;
    server.register_listener(tcp_listener, std::time::Duration::from_secs(5), 32);

    tracing::info!("Seeder running. Press Ctrl+C to exit.");

    // Run the server in the background, or block here?
    // Usually ServerFuture needs to be polled. `block_on` runs it.
    // We want to run it concurrently with ctrl_c.

    tokio::select! {
        dns_outcome = server.block_until_done() => {
            dns_outcome.wrap_err("DNS server crashed")?;
            tracing::info!("DNS server stopped, shutting down...");

            // Clean up metrics logger task
            metrics_handle.abort();

            // Brief delay to allow cleanup
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        prune_outcome = prune_task => {
            tracing::error!(?prune_outcome, "rate limiter prune task exited unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal, cleaning up...");

            // Abort the metrics logger task
            metrics_handle.abort();

            // Note: ServerFuture doesn't have a graceful shutdown method,
            // so we rely on the Drop implementation to clean up sockets

            // Brief delay to allow:
            // - Metrics logger task to finish aborting
            // - Any in-flight DNS responses to complete
            // - Metrics to flush (PrometheusBuilder handles this internally)
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            tracing::info!("Cleanup complete");
        }
    }

    Ok(())
}

fn log_crawler_status(book: &zebra_network::AddressBook, network: &Network) {
    let now = chrono::Utc::now();
    let banned: HashSet<IpAddr> = book.bans().keys().copied().collect();

    let mut servable_v4 = 0usize;
    let mut servable_v6 = 0usize;
    for meta in book.peers() {
        if eligibility::classify_peer(&meta, now, &banned, network).is_ok() {
            if meta.addr().ip().is_ipv4() {
                servable_v4 += 1;
            } else {
                servable_v6 += 1;
            }
        }
    }

    tracing::info!(
        total = book.len(),
        servable_v4,
        servable_v6,
        "crawler status"
    );
}

#[derive(Clone)]
pub(crate) struct SeederAuthority {
    latest_addresses: watch::Receiver<AddressRecords>,
    seed_domain: String,
    dns_ttl: u32,
    rate_limiter: Option<Arc<RateLimiter>>,
}

impl SeederAuthority {
    fn new(
        latest_addresses: watch::Receiver<AddressRecords>,
        seed_domain: String,
        dns_ttl: u32,
        rate_limiter: Option<Arc<RateLimiter>>,
    ) -> Self {
        Self {
            latest_addresses,
            seed_domain,
            dns_ttl,
            rate_limiter,
        }
    }
}

#[async_trait::async_trait]
impl RequestHandler for SeederAuthority {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &hickory_server::server::Request,
        response_handle: R,
    ) -> ResponseInfo {
        let span = info_span!("dns_query", client_addr = %request.src());
        async move { self.handle_request_inner(request, response_handle).await }
            .instrument(span)
            .await
    }
}

impl SeederAuthority {
    async fn handle_request_inner<R: ResponseHandler>(
        &self,
        request: &hickory_server::server::Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let builder = MessageResponseBuilder::from_message_request(request);
        let mut metadata = Metadata::response_from_request(&request.metadata);
        metadata.authoritative = true; // WE ARE THE AUTHORITY!

        // Rate limiting check
        if let Some(ref limiter) = self.rate_limiter {
            let client_ip = request.src().ip();

            if !limiter.check(client_ip) {
                tracing::warn!("Rate limit exceeded for {client_ip}");
                counter!("seeder_dns_rate_limited_total").increment(1);

                // Drop the request silently (no response to prevent amplification)
                return ResponseInfo::from(Header {
                    metadata,
                    counts: HeaderCounts::default(),
                });
            }
        }

        // Checking one query at a time standard
        // If multiple queries, usually we answer the first or all?
        // Standard DNS usually has 1 question.
        if let Some(query) = request.queries.queries().first() {
            let name = query.name();
            let record_type = query.query_type();

            // Check if we should answer this query
            let name_s = name.to_ascii();
            let name_norm = name_s.trim_end_matches('.');
            let seed_norm = self.seed_domain.trim_end_matches('.');

            if name_norm != seed_norm && !name_norm.ends_with(&format!(".{seed_norm}")) {
                // Return REFUSED
                metadata.response_code = ResponseCode::Refused;
                let response = builder.build(metadata, &[], &[], &[], &[]);
                return response_handle
                    .send_response(response)
                    .await
                    .unwrap_or_else(|_| {
                        ResponseInfo::from(Header {
                            metadata,
                            counts: HeaderCounts::default(),
                        })
                    });
            }

            // We only serve address records. Resolve the family-specific peer
            // set and metric label up front; any other query type gets an empty
            // NOERROR response.
            #[allow(
                clippy::wildcard_enum_match_arm,
                reason = "RecordType has many variants; the seeder serves only A and AAAA"
            )]
            let (peers, type_label) = match record_type {
                RecordType::A => (self.latest_addresses.borrow().ipv4.clone(), "A"),
                RecordType::AAAA => (self.latest_addresses.borrow().ipv6.clone(), "AAAA"),
                _ => {
                    let response = builder.build(metadata, &[], &[], &[], &[]);
                    return response_handle
                        .send_response(response)
                        .await
                        .unwrap_or_else(|_| {
                            ResponseInfo::from(Header {
                                metadata,
                                counts: HeaderCounts::default(),
                            })
                        });
                }
            };

            #[allow(
                clippy::cast_precision_loss,
                reason = "histogram sample of a small peer count"
            )]
            histogram!("seeder_dns_response_peers").record(peers.len() as f64);

            let records: Vec<Record> = peers
                .into_iter()
                .map(|addr| {
                    let rdata = match addr.ip() {
                        IpAddr::V4(ipv4) => RData::A(hickory_proto::rr::rdata::A(ipv4)),
                        IpAddr::V6(ipv6) => RData::AAAA(hickory_proto::rr::rdata::AAAA(ipv6)),
                    };
                    Record::from_rdata(name.clone().into(), self.dns_ttl, rdata)
                })
                .collect();

            counter!("seeder_dns_queries_total", &[("record_type", type_label)]).increment(1);

            let response = builder.build(metadata, records.iter(), &[], &[], &[]);
            return response_handle
                .send_response(response)
                .await
                .inspect_err(|e| {
                    tracing::warn!("failed to send DNS response: {e}");
                    counter!("seeder_dns_errors_total").increment(1);
                })
                .unwrap_or_else(|_| {
                    ResponseInfo::from(Header {
                        metadata,
                        counts: HeaderCounts::default(),
                    })
                });
        }

        // Default response (SERVFAIL or just empty user defined)
        // If we got here, we didn't return above.
        let response = builder.build(metadata, &[], &[], &[], &[]);
        response_handle
            .send_response(response)
            .await
            .unwrap_or_else(|_| {
                ResponseInfo::from(Header {
                    metadata,
                    counts: HeaderCounts::default(),
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    // Rate Limiter Tests
    #[test]
    fn test_rate_limiter_allows_normal_queries() {
        let limiter = RateLimiter::new_map(10, 20);
        let test_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        assert!(limiter.check(test_ip), "first query should be allowed");
        assert!(
            limiter.check(test_ip),
            "second query should be allowed within burst"
        );
    }

    #[test]
    fn test_rate_limiter_blocks_excessive_queries() {
        let limiter = RateLimiter::new_map(1, 2); // Very low limits for testing
        let test_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));

        // First two queries should pass (burst size = 2)
        assert!(limiter.check(test_ip), "Query 1 should pass");
        assert!(limiter.check(test_ip), "Query 2 should pass");

        // Third query should be rate limited
        assert!(!limiter.check(test_ip), "Query 3 should be rate limited");
    }

    #[test]
    fn test_rate_limiter_per_ip_isolation() {
        let limiter = RateLimiter::new_map(1, 1);
        let ip1 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));

        // Exhaust IP1's quota
        assert!(limiter.check(ip1), "IP1 first query should pass");
        assert!(!limiter.check(ip1), "IP1 second query should be blocked");

        // IP2 should still have quota
        assert!(limiter.check(ip2), "IP2 should have independent quota");
    }

    #[test]
    fn test_rate_limiter_ipv6_support() {
        let limiter = RateLimiter::new_map(10, 20);
        let ipv6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));

        assert!(limiter.check(ipv6), "IPv6 addresses should be supported");
    }

    // DNS Integration Tests
    // These run the full DNS server stack against a real resolver.

    use hickory_resolver::TokioResolver;
    use hickory_resolver::config::{
        ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts,
    };
    use hickory_resolver::net::runtime::TokioRuntimeProvider;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::TcpListener as TokioTcpListener;

    type TestResult = color_eyre::Result<()>;

    /// Bind a DNS server on a random local port for testing.
    async fn create_test_dns_server(
        authority: SeederAuthority,
    ) -> color_eyre::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
        let udp_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let server_addr = udp_socket.local_addr()?;
        let tcp_listener = TokioTcpListener::bind(server_addr).await?;

        let mut server = Server::new(authority);
        server.register_socket(udp_socket);
        server.register_listener(tcp_listener, Duration::from_secs(5), 32);

        let handle = tokio::spawn(async move {
            let _ = server.block_until_done().await;
        });

        // Give the server time to start.
        tokio::time::sleep(Duration::from_millis(50)).await;

        Ok((server_addr, handle))
    }

    /// Build a resolver pointing at our test server.
    #[allow(
        clippy::field_reassign_with_default,
        reason = "ResolverOpts is non_exhaustive, so struct-literal construction is not allowed"
    )]
    fn create_test_resolver(server_addr: SocketAddr) -> color_eyre::Result<TokioResolver> {
        let mut config = ResolverConfig::from_parts(None, vec![], vec![]);
        let mut connection = ConnectionConfig::udp();
        connection.port = server_addr.port();
        config.add_name_server(NameServerConfig::new(
            server_addr.ip(),
            true,
            vec![connection],
        ));

        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_secs(2);
        opts.attempts = 1;

        let resolver = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
            .with_options(opts)
            .build()?;
        Ok(resolver)
    }

    /// A watch receiver pre-loaded with `records` for DNS-serving tests.
    fn test_address_receiver(records: AddressRecords) -> watch::Receiver<AddressRecords> {
        let (sender, receiver) = watch::channel(records);
        // Leak the sender so the receiver stays open for the test's lifetime.
        std::mem::forget(sender);
        receiver
    }

    #[tokio::test]
    async fn test_dns_server_starts_and_responds() -> TestResult {
        let authority = SeederAuthority::new(
            test_address_receiver(AddressRecords::default()),
            "mainnet.seeder.test".to_string(),
            600,
            None,
        );

        let (server_addr, handle) = create_test_dns_server(authority).await?;
        let resolver = create_test_resolver(server_addr)?;

        // The query should complete even with no peers; an empty answer is fine.
        let result = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;

        match result {
            Ok(response) => {
                for record in response.answers() {
                    if let Some(ip) = record.data.ip_addr() {
                        assert!(ip.is_ipv4(), "A record should return IPv4");
                    }
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                assert!(
                    error_str.contains("no records") || error_str.contains("NoRecordsFound"),
                    "unexpected error: {e}"
                );
            }
        }

        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn test_dns_refused_for_wrong_domain() -> TestResult {
        let authority = SeederAuthority::new(
            test_address_receiver(AddressRecords::default()),
            "mainnet.seeder.test".to_string(),
            600,
            None,
        );

        let (server_addr, handle) = create_test_dns_server(authority).await?;
        let resolver = create_test_resolver(server_addr)?;

        let result = resolver
            .lookup("wrong.domain.test", hickory_proto::rr::RecordType::A)
            .await;
        assert!(result.is_err(), "query for wrong domain should fail");

        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn test_dns_rate_limiting_blocks_excessive_queries() -> TestResult {
        let rate_limiter = RateLimiter::new_map(1, 2);
        let authority = SeederAuthority::new(
            test_address_receiver(AddressRecords::default()),
            "mainnet.seeder.test".to_string(),
            600,
            Some(rate_limiter),
        );

        let (server_addr, handle) = create_test_dns_server(authority).await?;
        let resolver = create_test_resolver(server_addr)?;

        // The first two queries fit within the burst.
        let _ = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;
        let _ = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;

        // The third is dropped, so it times out (or errors).
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            resolver.lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A),
        )
        .await;
        let was_dropped = match result {
            Err(_elapsed) => true,
            Ok(lookup) => lookup.is_err(),
        };
        assert!(was_dropped, "third query should be rate limited");

        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn test_seeder_authority_handles_aaaa_queries() -> TestResult {
        let authority = SeederAuthority::new(
            test_address_receiver(AddressRecords::default()),
            "mainnet.seeder.test".to_string(),
            600,
            None,
        );

        let (server_addr, handle) = create_test_dns_server(authority).await?;
        let resolver = create_test_resolver(server_addr)?;

        let result = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::AAAA)
            .await;

        match result {
            Ok(response) => {
                for record in response.answers() {
                    if let Some(ip) = record.data.ip_addr() {
                        assert!(ip.is_ipv6(), "AAAA record should return IPv6");
                    }
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                assert!(
                    error_str.contains("no records") || error_str.contains("NoRecordsFound"),
                    "unexpected error: {e}"
                );
            }
        }

        handle.abort();
        Ok(())
    }

    /// End-to-end: a populated servable cache is served as exact A/AAAA records
    /// over the real DNS stack, split correctly by address family.
    #[tokio::test]
    async fn serves_cached_servable_addresses() -> TestResult {
        use std::collections::HashSet;

        use zebra_network::PeerSocketAddr;

        fn peer(ip: IpAddr) -> PeerSocketAddr {
            PeerSocketAddr::from(SocketAddr::new(ip, 8233))
        }

        let v4a = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        let v4b = IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8));
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));

        let records = AddressRecords {
            ipv4: vec![peer(v4a), peer(v4b)],
            ipv6: vec![peer(v6)],
        };

        let authority = SeederAuthority::new(
            test_address_receiver(records),
            "mainnet.seeder.test".to_string(),
            600,
            None,
        );
        let (server_addr, handle) = create_test_dns_server(authority).await?;
        let resolver = create_test_resolver(server_addr)?;

        let a = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await?;
        let got_v4: HashSet<IpAddr> = a
            .answers()
            .iter()
            .filter_map(|r| r.data.ip_addr())
            .collect();
        assert_eq!(
            got_v4,
            HashSet::from([v4a, v4b]),
            "A query should return exactly the cached IPv4 peers"
        );

        let aaaa = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::AAAA)
            .await?;
        let got_v6: HashSet<IpAddr> = aaaa
            .answers()
            .iter()
            .filter_map(|r| r.data.ip_addr())
            .collect();
        assert_eq!(
            got_v6,
            HashSet::from([v6]),
            "AAAA query should return exactly the cached IPv6 peers"
        );

        handle.abort();
        Ok(())
    }
}
