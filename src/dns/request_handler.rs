//! DNS request handling backed by the latest servable peer snapshot.

use std::net::IpAddr;

use color_eyre::eyre::{Context, Result};
use hickory_proto::{
    op::{Header, HeaderCounts, Metadata, ResponseCode},
    rr::{LowerName, Name, RData, Record, RecordType},
};
use hickory_server::{
    net::runtime::Time,
    server::{RequestHandler, ResponseHandler, ResponseInfo},
    zone_handler::MessageResponseBuilder,
};
use metrics::{counter, histogram};
use tokio::sync::watch;
use tracing::{Instrument, info_span};
use zebra_network::PeerSocketAddr;

use crate::{crawl::address_cache::ServablePeers, dns::rate_limiter::RateLimiter};

/// Hickory request handler for the seed domain.
#[derive(Clone)]
pub(crate) struct DnsRequestHandler {
    servable_peers: watch::Receiver<ServablePeers>,
    seed_domain: LowerName,
    dns_ttl: u32,
    rate_limiter: Option<RateLimiter>,
}

impl DnsRequestHandler {
    /// Build a DNS request handler using the latest servable peer snapshot.
    pub(crate) fn new(
        servable_peers: watch::Receiver<ServablePeers>,
        seed_domain: &str,
        dns_ttl: u32,
        rate_limiter: Option<RateLimiter>,
    ) -> Result<Self> {
        let seed_domain = Name::from_ascii(seed_domain)
            .wrap_err_with(|| format!("invalid seed domain `{seed_domain}`"))?;

        Ok(Self {
            servable_peers,
            seed_domain: LowerName::from(seed_domain),
            dns_ttl,
            rate_limiter,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DnsAnswer<'a> {
    Refused,
    Empty,
    Peers {
        peers: &'a [PeerSocketAddr],
        record_type_label: &'static str,
    },
}

fn resolve_dns_answer<'a>(
    query_name: &LowerName,
    record_type: RecordType,
    seed_domain: &LowerName,
    servable_peers: &'a ServablePeers,
) -> DnsAnswer<'a> {
    if !seed_domain.zone_of(query_name) {
        return DnsAnswer::Refused;
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "RecordType has many variants; the seeder serves only A and AAAA"
    )]
    match record_type {
        RecordType::A => DnsAnswer::Peers {
            peers: &servable_peers.ipv4,
            record_type_label: "A",
        },
        RecordType::AAAA => DnsAnswer::Peers {
            peers: &servable_peers.ipv6,
            record_type_label: "AAAA",
        },
        _ => DnsAnswer::Empty,
    }
}

#[async_trait::async_trait]
impl RequestHandler for DnsRequestHandler {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &hickory_server::server::Request,
        response_handle: R,
    ) -> ResponseInfo {
        let span = info_span!("dns_query", client_addr = %request.src());
        async move { self.answer_request(request, response_handle).await }
            .instrument(span)
            .await
    }
}

impl DnsRequestHandler {
    async fn answer_request<R: ResponseHandler>(
        &self,
        request: &hickory_server::server::Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let builder = MessageResponseBuilder::from_message_request(request);
        let mut metadata = Metadata::response_from_request(&request.metadata);
        metadata.authoritative = true;

        if let Some(ref limiter) = self.rate_limiter {
            let client_ip = request.src().ip();

            if !limiter.check(client_ip) {
                tracing::warn!("Rate limit exceeded for {client_ip}");
                counter!("seeder_dns_rate_limited_total").increment(1);

                // Drop the request silently to prevent amplification.
                return ResponseInfo::from(Header {
                    metadata,
                    counts: HeaderCounts::default(),
                });
            }
        }

        #[allow(
            clippy::option_if_let_else,
            reason = "the query branch mutates response metadata, so if-let keeps that side effect visible"
        )]
        let records: Vec<Record> = if let Some(query) = request.queries.queries().first() {
            let name = query.name();
            let record_type = query.query_type();

            let servable_peers = self.servable_peers.borrow();
            match resolve_dns_answer(name, record_type, &self.seed_domain, &servable_peers) {
                DnsAnswer::Refused => {
                    metadata.response_code = ResponseCode::Refused;
                    Vec::new()
                }
                DnsAnswer::Empty => Vec::new(),
                DnsAnswer::Peers {
                    peers,
                    record_type_label,
                } => {
                    #[allow(
                        clippy::cast_precision_loss,
                        reason = "histogram sample of a small peer count"
                    )]
                    histogram!("seeder_dns_response_peers").record(peers.len() as f64);
                    counter!(
                        "seeder_dns_queries_total",
                        &[("record_type", record_type_label)]
                    )
                    .increment(1);

                    peers
                        .iter()
                        .copied()
                        .map(|addr| {
                            let rdata = match addr.ip() {
                                IpAddr::V4(ipv4) => RData::A(hickory_proto::rr::rdata::A(ipv4)),
                                IpAddr::V6(ipv6) => {
                                    RData::AAAA(hickory_proto::rr::rdata::AAAA(ipv6))
                                }
                            };
                            Record::from_rdata(name.clone().into(), self.dns_ttl, rdata)
                        })
                        .collect()
                }
            }
        } else {
            Vec::new()
        };

        let response = builder.build(metadata, records.iter(), &[], &[], &[]);
        response_handle
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
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        time::Duration,
    };

    use hickory_resolver::{
        TokioResolver,
        config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts},
        net::runtime::TokioRuntimeProvider,
    };
    use hickory_server::server::Server;
    use tokio::net::{TcpListener as TokioTcpListener, UdpSocket};
    use zebra_network::PeerSocketAddr;

    use super::*;
    use crate::{config::RateLimitConfig, dns::rate_limiter::RateLimiter};

    type TestResult = color_eyre::Result<()>;

    async fn create_test_dns_server(
        request_handler: DnsRequestHandler,
    ) -> color_eyre::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
        let udp_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let server_addr = udp_socket.local_addr()?;
        let tcp_listener = TokioTcpListener::bind(server_addr).await?;

        let mut server = Server::new(request_handler);
        server.register_socket(udp_socket);
        server.register_listener(tcp_listener, Duration::from_secs(5), 32);

        let handle = tokio::spawn(async move {
            let _ = server.block_until_done().await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        Ok((server_addr, handle))
    }

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

    fn servable_peers_receiver(servable_peers: ServablePeers) -> watch::Receiver<ServablePeers> {
        let (_sender, receiver) = watch::channel(servable_peers);
        receiver
    }

    fn peer(ip: IpAddr) -> PeerSocketAddr {
        PeerSocketAddr::from(SocketAddr::new(ip, 8233))
    }

    fn lower_name(name: &str) -> color_eyre::Result<LowerName> {
        Ok(LowerName::from(Name::from_ascii(name)?))
    }

    fn servable_peers() -> ServablePeers {
        ServablePeers {
            ipv4: vec![peer(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))],
            ipv6: vec![peer(IpAddr::V6(Ipv6Addr::new(
                0x2001, 0x0db8, 0, 0, 0, 0, 0, 1,
            )))],
        }
    }

    #[test]
    fn answers_apex_queries_for_seed_domain() -> TestResult {
        let peers = servable_peers();
        let answer = resolve_dns_answer(
            &lower_name("mainnet.seeder.test")?,
            RecordType::A,
            &lower_name("mainnet.seeder.test")?,
            &peers,
        );

        assert_eq!(
            answer,
            DnsAnswer::Peers {
                peers: &peers.ipv4,
                record_type_label: "A"
            }
        );
        Ok(())
    }

    #[test]
    fn answers_subdomain_queries_under_seed_domain() -> TestResult {
        let peers = servable_peers();
        let answer = resolve_dns_answer(
            &lower_name("node.mainnet.seeder.test")?,
            RecordType::AAAA,
            &lower_name("mainnet.seeder.test")?,
            &peers,
        );

        assert_eq!(
            answer,
            DnsAnswer::Peers {
                peers: &peers.ipv6,
                record_type_label: "AAAA"
            }
        );
        Ok(())
    }

    #[test]
    fn answers_trailing_dot_queries_for_seed_domain() -> TestResult {
        let peers = servable_peers();
        let answer = resolve_dns_answer(
            &lower_name("mainnet.seeder.test.")?,
            RecordType::A,
            &lower_name("mainnet.seeder.test")?,
            &peers,
        );

        assert_eq!(
            answer,
            DnsAnswer::Peers {
                peers: &peers.ipv4,
                record_type_label: "A"
            }
        );
        Ok(())
    }

    #[test]
    fn answers_mixed_case_seed_domain_queries() -> TestResult {
        let peers = servable_peers();
        let answer = resolve_dns_answer(
            &lower_name("mainnet.seeder.test.")?,
            RecordType::A,
            &lower_name("MainNet.Seeder.Test")?,
            &peers,
        );

        assert_eq!(
            answer,
            DnsAnswer::Peers {
                peers: &peers.ipv4,
                record_type_label: "A"
            }
        );
        Ok(())
    }

    #[test]
    fn refuses_queries_outside_seed_domain() -> TestResult {
        let peers = servable_peers();

        assert_eq!(
            resolve_dns_answer(
                &lower_name("wrong.domain.test")?,
                RecordType::A,
                &lower_name("mainnet.seeder.test")?,
                &peers,
            ),
            DnsAnswer::Refused
        );
        assert_eq!(
            resolve_dns_answer(
                &lower_name("evilmainnet.seeder.test")?,
                RecordType::A,
                &lower_name("mainnet.seeder.test")?,
                &peers,
            ),
            DnsAnswer::Refused
        );
        Ok(())
    }

    #[test]
    fn returns_empty_answer_for_unsupported_record_types() -> TestResult {
        let peers = servable_peers();

        assert_eq!(
            resolve_dns_answer(
                &lower_name("mainnet.seeder.test")?,
                RecordType::NS,
                &lower_name("mainnet.seeder.test")?,
                &peers,
            ),
            DnsAnswer::Empty
        );
        assert_eq!(
            resolve_dns_answer(
                &lower_name("mainnet.seeder.test")?,
                RecordType::TXT,
                &lower_name("mainnet.seeder.test")?,
                &peers,
            ),
            DnsAnswer::Empty
        );
        Ok(())
    }

    #[test]
    fn answers_supported_query_with_empty_peer_cache() -> TestResult {
        let peers = ServablePeers::default();
        let answer = resolve_dns_answer(
            &lower_name("mainnet.seeder.test")?,
            RecordType::A,
            &lower_name("mainnet.seeder.test")?,
            &peers,
        );

        assert_eq!(
            answer,
            DnsAnswer::Peers {
                peers: &peers.ipv4,
                record_type_label: "A"
            }
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_seed_domain() {
        let result = DnsRequestHandler::new(
            servable_peers_receiver(ServablePeers::default()),
            "not a domain",
            600,
            None,
        );

        assert!(result.is_err(), "invalid seed domain should fail startup");
    }

    #[tokio::test]
    async fn test_dns_server_starts_and_responds() -> TestResult {
        let request_handler = DnsRequestHandler::new(
            servable_peers_receiver(ServablePeers::default()),
            "mainnet.seeder.test",
            600,
            None,
        )?;

        let (server_addr, handle) = create_test_dns_server(request_handler).await?;
        let resolver = create_test_resolver(server_addr)?;

        let lookup = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;

        match lookup {
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
        let request_handler = DnsRequestHandler::new(
            servable_peers_receiver(ServablePeers::default()),
            "mainnet.seeder.test",
            600,
            None,
        )?;

        let (server_addr, handle) = create_test_dns_server(request_handler).await?;
        let resolver = create_test_resolver(server_addr)?;

        let lookup = resolver
            .lookup("wrong.domain.test", hickory_proto::rr::RecordType::A)
            .await;
        assert!(lookup.is_err(), "query for wrong domain should fail");

        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn test_dns_rate_limiting_blocks_excessive_queries() -> TestResult {
        let rate_limiter = RateLimiter::new(&RateLimitConfig {
            queries_per_second: 1,
            burst_size: 2,
        });
        let request_handler = DnsRequestHandler::new(
            servable_peers_receiver(ServablePeers::default()),
            "mainnet.seeder.test",
            600,
            Some(rate_limiter),
        )?;

        let (server_addr, handle) = create_test_dns_server(request_handler).await?;
        let resolver = create_test_resolver(server_addr)?;

        let _ = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;
        let _ = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A)
            .await;

        let timeout_outcome = tokio::time::timeout(
            Duration::from_millis(500),
            resolver.lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::A),
        )
        .await;
        let was_dropped = match timeout_outcome {
            Err(_elapsed) => true,
            Ok(lookup) => lookup.is_err(),
        };
        assert!(was_dropped, "third query should be rate limited");

        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn test_dns_request_handler_handles_aaaa_queries() -> TestResult {
        let request_handler = DnsRequestHandler::new(
            servable_peers_receiver(ServablePeers::default()),
            "mainnet.seeder.test",
            600,
            None,
        )?;

        let (server_addr, handle) = create_test_dns_server(request_handler).await?;
        let resolver = create_test_resolver(server_addr)?;

        let lookup = resolver
            .lookup("mainnet.seeder.test", hickory_proto::rr::RecordType::AAAA)
            .await;

        match lookup {
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

    #[tokio::test]
    async fn serves_cached_servable_peers() -> TestResult {
        let v4a = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        let v4b = IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8));
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));

        let servable_peers = ServablePeers {
            ipv4: vec![peer(v4a), peer(v4b)],
            ipv6: vec![peer(v6)],
        };

        let request_handler = DnsRequestHandler::new(
            servable_peers_receiver(servable_peers),
            "mainnet.seeder.test",
            600,
            None,
        )?;
        let (server_addr, handle) = create_test_dns_server(request_handler).await?;
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
