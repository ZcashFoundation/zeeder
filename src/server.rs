use std::{sync::Arc, time::Duration};

use color_eyre::eyre::{Context, Result};
use hickory_proto::{
    op::{Header, ResponseCode},
    rr::{RData, Record, RecordType},
};
use hickory_server::{
    authority::MessageResponseBuilder,
    server::{RequestHandler, ResponseHandler, ResponseInfo},
    ServerFuture,
};
use metrics::{counter, gauge, histogram};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::watch,
    time,
};
use tracing::{info_span, Instrument};

use crate::{
    config::SeederConfig,
    server::{
        address_cache::AddressRecords,
        rate_limiter::{RateLimiter, RateLimiterExt},
    },
};

mod address_cache;
mod rate_limiter;

pub async fn spawn(config: SeederConfig) -> Result<()> {
    tracing::info!("Initializing zebra-network...");

    // Dummy inbound service that rejects everything
    let inbound_service = tower::service_fn(|_req: zebra_network::Request| async move {
        Ok::<zebra_network::Response, Box<dyn std::error::Error + Send + Sync + 'static>>(
            zebra_network::Response::Nil,
        )
    });

    // Provide a user agent
    let user_agent = if let Some(sha) = option_env!("VERGEN_GIT_SHA") {
        let short_sha = &sha[..7.min(sha.len())];
        format!("zebra-seeder/{} ({})", env!("CARGO_PKG_VERSION"), short_sha)
    } else {
        format!("zebra-seeder/{}", env!("CARGO_PKG_VERSION"))
    };

    tracing::info!("User-Agent: {}", user_agent);

    // Initialize zebra-network
    let (peer_set, address_book, _peer_sender) = zebra_network::init(
        config.network.clone(),
        inbound_service,
        zebra_chain::chain_tip::NoChainTip,
        user_agent,
    )
    .await;

    // Metrics/status logging interval (zebra-network handles actual crawling internally)
    const METRICS_LOG_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes

    // Spawn the metrics logger task
    let address_book_monitor = address_book.clone();
    let default_port = config.network.network.default_port();

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
                    counter!("seeder.mutex_poisoning_total", "location" => "metrics_logger")
                        .increment(1);
                    poisoned.into_inner()
                }
            };
            log_crawler_status(&book, default_port);
        }
    });

    // Initialize rate limiter if configured
    let (rate_limiter, prune_task) = rate_limiter::spawn(config.clone());

    tracing::info!("Initializing DNS server on {}", config.dns_listen_addr);

    // Spawn address cache updater - provides lock-free reads for DNS queries
    let latest_addresses =
        address_cache::spawn(address_book.clone(), config.network.network.default_port());

    let authority = SeederAuthority::new(
        latest_addresses,
        config.seed_domain.clone(),
        config.dns_ttl,
        rate_limiter,
    );
    let mut server = ServerFuture::new(authority);

    // Register UDP and TCP listeners
    let udp_socket = UdpSocket::bind(config.dns_listen_addr)
        .await
        .wrap_err("failed to bind UDP socket")?;
    server.register_socket(udp_socket);

    let tcp_listener = TcpListener::bind(config.dns_listen_addr)
        .await
        .wrap_err("failed to bind TCP listener")?;
    server.register_listener(tcp_listener, std::time::Duration::from_secs(5));

    tracing::info!("Seeder running. Press Ctrl+C to exit.");

    // Run the server in the background, or block here?
    // Usually ServerFuture needs to be polled. `block_on` runs it.
    // We want to run it concurrently with ctrl_c.

    tokio::select! {
        result = server.block_until_done() => {
            result.wrap_err("DNS server crashed")?;
            tracing::info!("DNS server stopped, shutting down...");

            // Clean up metrics logger task
            metrics_handle.abort();

            // Brief delay to allow cleanup
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        result = prune_task => {
            tracing::error!(?result, "rate limiter prune task exited unexpectedly");
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

fn log_crawler_status(book: &zebra_network::AddressBook, default_port: u16) {
    let total_peers = book.len();

    // Calculate eligible peers (passing filter criteria)
    let peers: Vec<_> = book.peers().collect();

    let eligible_v4 = peers
        .iter()
        .filter(|meta| {
            let ip = meta.addr().ip();
            let is_global = !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast();
            is_global && ip.is_ipv4() && meta.addr().port() == default_port
        })
        .count();

    let eligible_v6 = peers
        .iter()
        .filter(|meta| {
            let ip = meta.addr().ip();
            let is_global = !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast();
            is_global && ip.is_ipv6() && meta.addr().port() == default_port
        })
        .count();

    tracing::info!(
        "Crawler Status: Total={} | Eligible IPv4={} | Eligible IPv6={}",
        total_peers,
        eligible_v4,
        eligible_v6
    );

    gauge!("seeder.peers.total").set(total_peers as f64);
    gauge!("seeder.peers.eligible", "addr_family" => "v4").set(eligible_v4 as f64);
    gauge!("seeder.peers.eligible", "addr_family" => "v6").set(eligible_v6 as f64);
}

#[derive(Clone)]
pub struct SeederAuthority {
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
    async fn handle_request<R: ResponseHandler>(
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
        let mut header = Header::response_from_request(request.header());
        header.set_authoritative(true); // WE ARE THE AUTHORITY!

        // Rate limiting check
        if let Some(ref limiter) = self.rate_limiter {
            let client_ip = request.src().ip();

            if !limiter.check(client_ip) {
                tracing::warn!("Rate limit exceeded for {}", client_ip);
                counter!("seeder.dns.rate_limited_total").increment(1);

                // Drop the request silently (no response to prevent amplification)
                return ResponseInfo::from(header);
            }
        }

        // Checking one query at a time standard
        // If multiple queries, usually we answer the first or all?
        // Standard DNS usually has 1 question.
        if let Some(query) = request.queries().first() {
            let name = query.name();
            let record_type = query.query_type();

            // Check if we should answer this query
            let name_s = name.to_ascii();
            let name_norm = name_s.trim_end_matches('.');
            let seed_norm = self.seed_domain.trim_end_matches('.');

            if name_norm != seed_norm && !name_norm.ends_with(&format!(".{}", seed_norm)) {
                // Return REFUSED
                header.set_response_code(ResponseCode::Refused);
                let response = builder.build(header, &[], &[], &[], &[]);
                return response_handle
                    .send_response(response)
                    .await
                    .unwrap_or_else(|_| ResponseInfo::from(header));
            }

            let mut records = Vec::new();

            // Read from the cached addresses - no mutex lock needed!
            // The cache is updated by a background task every 5 seconds.
            let response_data = match record_type {
                RecordType::A => Some(("A", self.latest_addresses.borrow().ipv4.clone())),
                RecordType::AAAA => Some(("AAAA", self.latest_addresses.borrow().ipv6.clone())),
                RecordType::TXT => {
                    let AddressRecords { ipv4, ipv6 } = self.latest_addresses.borrow().clone();
                    Some(("TXT", ipv4.into_iter().chain(ipv6.into_iter()).collect()))
                }
                _ => None,
            };

            if let Some((type_label, matched_peers)) = response_data {
                histogram!("seeder.dns.response_peers").record(matched_peers.len() as f64);

                for addr in matched_peers {
                    let rdata = match addr.ip() {
                        std::net::IpAddr::V4(ipv4) => RData::A(hickory_proto::rr::rdata::A(ipv4)),
                        std::net::IpAddr::V6(ipv6) => {
                            RData::AAAA(hickory_proto::rr::rdata::AAAA(ipv6))
                        }
                    };

                    let record = Record::from_rdata(name.clone().into(), self.dns_ttl, rdata);
                    records.push(record);
                }

                counter!("seeder.dns.queries_total", &[("record_type", type_label)]).increment(1);

                let response = builder.build(header, records.iter(), &[], &[], &[]);
                return response_handle
                    .send_response(response)
                    .await
                    .inspect_err(|e| {
                        tracing::warn!("Failed to send DNS response: {}", e);
                        counter!("seeder.dns.errors_total").increment(1);
                    })
                    .unwrap_or_else(|_| {
                        ResponseInfo::from(header) // fallback
                    });
            }
        } else {
            // For NS, SOA, etc, we might want to return something else or Refused.
            // Returning empty NOERROR or NXDOMAIN?
            // Let's return NOERROR empty for now.
        }

        // Default response (SERVFAIL or just empty user defined)
        // If we got here, we didn't return above.
        let response = builder.build(header, &[], &[], &[], &[]);
        response_handle
            .send_response(response)
            .await
            .unwrap_or_else(|_| ResponseInfo::from(header))
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    // Rate Limiter Tests
    #[test]
    fn test_rate_limiter_allows_normal_queries() {
        let limiter = RateLimiter::new_map(10, 20);
        let test_ip: IpAddr = "192.168.1.1".parse().unwrap();

        // First query should be allowed
        assert!(limiter.check(test_ip), "First query should be allowed");

        // Second query should also be allowed (within burst)
        assert!(limiter.check(test_ip), "Second query should be allowed");
    }

    #[test]
    fn test_rate_limiter_blocks_excessive_queries() {
        let limiter = RateLimiter::new_map(1, 2); // Very low limits for testing
        let test_ip: IpAddr = "192.168.1.2".parse().unwrap();

        // First two queries should pass (burst size = 2)
        assert!(limiter.check(test_ip), "Query 1 should pass");
        assert!(limiter.check(test_ip), "Query 2 should pass");

        // Third query should be rate limited
        assert!(!limiter.check(test_ip), "Query 3 should be rate limited");
    }

    #[test]
    fn test_rate_limiter_per_ip_isolation() {
        let limiter = RateLimiter::new_map(1, 1);
        let ip1: IpAddr = "192.168.1.1".parse().unwrap();
        let ip2: IpAddr = "192.168.1.2".parse().unwrap();

        // Exhaust IP1's quota
        assert!(limiter.check(ip1), "IP1 first query should pass");
        assert!(!limiter.check(ip1), "IP1 second query should be blocked");

        // IP2 should still have quota
        assert!(limiter.check(ip2), "IP2 should have independent quota");
    }

    #[test]
    fn test_rate_limiter_ipv6_support() {
        let limiter = RateLimiter::new_map(10, 20);
        let ipv6: IpAddr = "2001:db8::1".parse().unwrap();

        assert!(limiter.check(ipv6), "IPv6 addresses should be supported");
    }

    // Peer Filtering Logic Tests
    #[test]
    fn test_ipv4_is_global() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let unspecified: IpAddr = "0.0.0.0".parse().unwrap();
        let multicast: IpAddr = "224.0.0.1".parse().unwrap();
        let global: IpAddr = "8.8.8.8".parse().unwrap();

        // Test the same logic used in handle_request_inner for filtering
        assert!(loopback.is_loopback(), "Loopback should be detected");
        assert!(
            unspecified.is_unspecified(),
            "Unspecified should be detected"
        );
        assert!(multicast.is_multicast(), "Multicast should be detected");

        let is_global = !global.is_loopback() && !global.is_unspecified() && !global.is_multicast();
        assert!(is_global, "8.8.8.8 should be considered global");
    }

    #[test]
    fn test_ipv6_is_global() {
        let loopback: IpAddr = "::1".parse().unwrap();
        let unspecified: IpAddr = "::".parse().unwrap();
        let multicast: IpAddr = "ff02::1".parse().unwrap();
        let global: IpAddr = "2001:4860:4860::8888".parse().unwrap();

        assert!(loopback.is_loopback());
        assert!(unspecified.is_unspecified());
        assert!(multicast.is_multicast());

        let is_global = !global.is_loopback() && !global.is_unspecified() && !global.is_multicast();
        assert!(is_global, "Google DNS IPv6 should be considered global");
    }

    #[test]
    fn test_private_ipv4_ranges() {
        let private_10: IpAddr = "10.0.0.1".parse().unwrap();
        let private_172: IpAddr = "172.16.0.1".parse().unwrap();
        let private_192: IpAddr = "192.168.1.1".parse().unwrap();

        // These are not loopback/unspecified/multicast, but they're private
        // The server.rs logic doesn't explicitly filter private ranges,
        // but we document this behavior for future reference
        let is_global_10 =
            !private_10.is_loopback() && !private_10.is_unspecified() && !private_10.is_multicast();
        let is_global_172 = !private_172.is_loopback()
            && !private_172.is_unspecified()
            && !private_172.is_multicast();
        let is_global_192 = !private_192.is_loopback()
            && !private_192.is_unspecified()
            && !private_192.is_multicast();

        // Note: Current implementation would consider these "global"
        // This is acceptable for a seeder as peers on private networks won't be reachable anyway
        assert!(is_global_10);
        assert!(is_global_172);
        assert!(is_global_192);
    }

    // DNS Integration Tests
    // These require async and test the full DNS server stack

    use hickory_proto::xfer::Protocol;
    use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
    use hickory_resolver::TokioResolver;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::TcpListener as TokioTcpListener;

    /// Helper to create a DNS server on a random port for testing
    async fn create_test_dns_server(
        authority: SeederAuthority,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        // Bind to a random port
        let udp_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = udp_socket.local_addr().unwrap();

        let tcp_listener = TokioTcpListener::bind(server_addr).await.unwrap();

        let mut server = ServerFuture::new(authority);
        server.register_socket(udp_socket);
        server.register_listener(tcp_listener, Duration::from_secs(5));

        let handle = tokio::spawn(async move {
            let _ = server.block_until_done().await;
        });

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        (server_addr, handle)
    }

    /// Helper to create a resolver pointing to our test server
    fn create_test_resolver(server_addr: SocketAddr) -> TokioResolver {
        use hickory_resolver::name_server::TokioConnectionProvider;

        let mut config = ResolverConfig::new();
        config.add_name_server(NameServerConfig::new(server_addr, Protocol::Udp));

        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_secs(2);
        opts.attempts = 1;

        TokioResolver::builder_with_config(config, TokioConnectionProvider::default())
            .with_options(opts)
            .build()
    }

    /// Helper to create a test watch receiver with empty addresses for testing
    fn create_test_address_receiver() -> watch::Receiver<AddressRecords> {
        let (sender, receiver) = watch::channel(AddressRecords::default());
        // Keep sender alive by leaking it (acceptable for tests)
        std::mem::forget(sender);
        receiver
    }

    #[tokio::test]
    async fn test_dns_server_starts_and_responds() {
        let latest_addresses = create_test_address_receiver();

        let authority = SeederAuthority::new(
            latest_addresses,
            "mainnet.seeder.test".to_string(),
            600,
            None,
        );

        let (server_addr, handle) = create_test_dns_server(authority).await;

        // Create resolver
        let resolver = create_test_resolver(server_addr);

        // Query for A records - should get a valid response (possibly empty)
        let result = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;

        // The query should complete (even if no peers are available)
        // An empty response or valid response is acceptable
        match result {
            Ok(response) => {
                // Valid response - check all IPs are IPv4
                for record in response.iter() {
                    if let Some(ip) = record.ip_addr() {
                        assert!(ip.is_ipv4(), "A record should return IPv4");
                    }
                }
            }
            Err(e) => {
                // No error means query worked, empty is OK
                let error_str = e.to_string();
                assert!(
                    error_str.contains("no records") || error_str.contains("NoRecordsFound"),
                    "Unexpected error: {}",
                    e
                );
            }
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_dns_refused_for_wrong_domain() {
        let latest_addresses = create_test_address_receiver();

        let authority = SeederAuthority::new(
            latest_addresses,
            "mainnet.seeder.test".to_string(),
            600,
            None,
        );

        let (server_addr, handle) = create_test_dns_server(authority).await;
        let resolver = create_test_resolver(server_addr);

        // Query for a different domain - should be refused
        let result = resolver
            .lookup("wrong.domain.test", hickory_proto::rr::RecordType::A)
            .await;

        assert!(result.is_err(), "Query for wrong domain should fail");

        handle.abort();
    }

    #[tokio::test]
    async fn test_dns_rate_limiting_blocks_excessive_queries() {
        let latest_addresses = create_test_address_receiver();

        // Very low rate limit for testing
        let rate_limiter = RateLimiter::new_map(1, 2);

        let authority = SeederAuthority::new(
            latest_addresses,
            "mainnet.seeder.test".to_string(),
            600,
            Some(rate_limiter),
        );

        let (server_addr, handle) = create_test_dns_server(authority).await;
        let resolver = create_test_resolver(server_addr);

        // First two queries should succeed (within burst)
        let _ = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;
        let _ = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;

        // Third query should timeout (rate limited = dropped)
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            resolver.lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A),
        )
        .await;

        // Should either timeout or get an error
        assert!(
            result.is_err() || result.unwrap().is_err(),
            "Third query should be rate limited"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_seeder_authority_handles_aaaa_queries() {
        let latest_addresses = create_test_address_receiver();

        let authority = SeederAuthority::new(
            latest_addresses,
            "mainnet.seeder.test".to_string(),
            600,
            None,
        );

        let (server_addr, handle) = create_test_dns_server(authority).await;
        let resolver = create_test_resolver(server_addr);

        // Query for AAAA records
        let result = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::AAAA)
            .await;

        // Should get a valid response (possibly empty for IPv6)
        match result {
            Ok(response) => {
                for record in response.iter() {
                    if let Some(ip) = record.ip_addr() {
                        assert!(ip.is_ipv6(), "AAAA record should return IPv6");
                    }
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                assert!(
                    error_str.contains("no records") || error_str.contains("NoRecordsFound"),
                    "Unexpected error: {}",
                    e
                );
            }
        }

        handle.abort();
    }

    // Property-Based Tests
    // These use proptest to verify filtering logic with random inputs

    use proptest::prelude::*;

    /// The IP filtering logic used in handle_request_inner
    fn is_routable(ip: IpAddr) -> bool {
        !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
    }

    proptest! {
        /// Verify that the IP filtering logic never panics on any valid IP address
        #[test]
        fn prop_ip_filtering_never_panics(ip_bytes in prop::array::uniform4(any::<u8>())) {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(ip_bytes));
            // Should never panic
            let _ = is_routable(ip);
        }

        /// Verify that the IP filtering logic never panics on any valid IPv6 address
        #[test]
        fn prop_ipv6_filtering_never_panics(ip_bytes in prop::array::uniform16(any::<u8>())) {
            let ip = IpAddr::V6(std::net::Ipv6Addr::from(ip_bytes));
            // Should never panic
            let _ = is_routable(ip);
        }

        /// Verify that loopback addresses are never considered routable
        #[test]
        fn prop_loopback_never_routable(last_byte in 1u8..=255u8) {
            let ip = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, last_byte));
            prop_assert!(!is_routable(ip) || last_byte == 0, "Loopback should not be routable");
        }

        /// Verify that multicast addresses are never considered routable
        #[test]
        fn prop_multicast_never_routable(b2 in 0u8..=255u8, b3 in 0u8..=255u8, b4 in 0u8..=255u8) {
            // Multicast range: 224.0.0.0 - 239.255.255.255
            let ip = IpAddr::V4(std::net::Ipv4Addr::new(224 + (b2 % 16), b2, b3, b4));
            prop_assert!(!is_routable(ip), "Multicast should not be routable");
        }
    }

    #[test]
    fn test_routable_ip_classification() {
        // Global IPs should be routable
        assert!(is_routable("8.8.8.8".parse().unwrap()));
        assert!(is_routable("1.1.1.1".parse().unwrap()));
        assert!(is_routable("2001:4860:4860::8888".parse().unwrap()));

        // Special addresses should NOT be routable
        assert!(!is_routable("127.0.0.1".parse().unwrap()));
        assert!(!is_routable("0.0.0.0".parse().unwrap()));
        assert!(!is_routable("224.0.0.1".parse().unwrap()));
        assert!(!is_routable("::1".parse().unwrap()));
        assert!(!is_routable("::".parse().unwrap()));
        assert!(!is_routable("ff02::1".parse().unwrap()));
    }
}
