//! DNS request handling backed by the latest servable peer snapshot.
//!
//! One handler serves every configured zone on a single listener. Each query is
//! routed to the zone whose domain contains the query name; names outside every
//! zone are REFUSED.

use std::net::IpAddr;

use color_eyre::eyre::{Context, Result, ensure};
use hickory_proto::{
    op::{Header, HeaderCounts, Metadata, ResponseCode},
    rr::{
        LowerName, Name, RData, Record, RecordType,
        rdata::{NS, SOA},
    },
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

use crate::{
    config::ZcashNetwork,
    crawl::address_cache::ServablePeers,
    dns::rate_limiter::RateLimiter,
    metrics::{
        DNS_ERRORS_TOTAL, DNS_QUERIES_TOTAL, DNS_RATE_LIMITED_TOTAL, DNS_RESPONSE_PEERS,
        LABEL_NETWORK, LABEL_RECORD_TYPE, RECORD_TYPE_A, RECORD_TYPE_AAAA, RECORD_TYPE_NS,
        RECORD_TYPE_OTHER, RECORD_TYPE_SOA,
    },
};

const SOA_SERIAL: u32 = 1;
const SOA_REFRESH_SECONDS: i32 = 3_600;
const SOA_RETRY_SECONDS: i32 = 600;
const SOA_EXPIRE_SECONDS: i32 = 86_400;

/// Hickory request handler that routes each query to its matching seed zone.
#[derive(Clone)]
pub(crate) struct DnsRequestHandler {
    zones: Vec<SeedZone>,
    rate_limiter: Option<RateLimiter>,
}

/// One network's served DNS zone: its static metadata plus a live feed of that
/// network's servable peers.
#[derive(Clone)]
pub(crate) struct SeedZone {
    network: ZcashNetwork,
    domain: LowerName,
    records: ZoneRecords,
    ttl: u32,
    servable_peers: watch::Receiver<ServablePeers>,
}

#[derive(Clone)]
struct ZoneRecords {
    soa: Record,
    nameserver: Record,
}

impl SeedZone {
    /// Parse a zone's domain and static metadata, binding it to the network's
    /// live servable-peer feed.
    pub(crate) fn new(
        network: ZcashNetwork,
        seed_domain: &str,
        nameserver: &str,
        dns_ttl: u32,
        servable_peers: watch::Receiver<ServablePeers>,
    ) -> Result<Self> {
        let seed_domain = parse_absolute_name("seed domain", seed_domain)?;
        let nameserver_name = parse_absolute_name("nameserver", nameserver)?;
        let seed_domain_lower = LowerName::from(seed_domain.clone());
        let nameserver_lower = LowerName::from(nameserver_name.clone());
        ensure!(
            !seed_domain_lower.zone_of(&nameserver_lower),
            "nameserver must be outside the seed domain because Zeeder does not serve address records for nameserver hostnames"
        );
        let records = ZoneRecords::new(seed_domain, nameserver_name, dns_ttl)?;

        Ok(Self {
            network,
            domain: seed_domain_lower,
            records,
            ttl: dns_ttl,
            servable_peers,
        })
    }

    /// This zone's network paired with its live servable-peer feed, for the
    /// health endpoint's readiness check.
    pub(crate) fn readiness(&self) -> (ZcashNetwork, watch::Receiver<ServablePeers>) {
        (self.network, self.servable_peers.clone())
    }

    /// Whether this zone is authoritative for `query_name`.
    fn contains(&self, query_name: &LowerName) -> bool {
        self.domain.zone_of(query_name)
    }

    /// Decide the answer for an in-zone query, reading this zone's peer feed.
    fn resolve_answer<'a>(
        &self,
        query_name: &LowerName,
        record_type: RecordType,
        servable_peers: &'a ServablePeers,
    ) -> ZoneAnswer<'a> {
        if query_name.num_labels() != self.domain.num_labels() {
            return ZoneAnswer::NoData;
        }

        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "RecordType has many variants; the seeder serves only A and AAAA"
        )]
        match record_type {
            RecordType::A => resolve_peer_answer(&servable_peers.ipv4),
            RecordType::AAAA => resolve_peer_answer(&servable_peers.ipv6),
            RecordType::NS => ZoneAnswer::Nameserver,
            RecordType::SOA => ZoneAnswer::StartOfAuthority,
            _ => ZoneAnswer::NoData,
        }
    }

    fn response_records_for_answer(
        &self,
        query_name: &LowerName,
        answer: ZoneAnswer<'_>,
    ) -> DnsResponseRecords {
        match answer {
            ZoneAnswer::NoData => DnsResponseRecords {
                answer_records: Vec::new(),
                soa_records: vec![self.records.soa.clone()],
            },
            ZoneAnswer::Nameserver => DnsResponseRecords {
                answer_records: vec![self.records.nameserver.clone()],
                soa_records: Vec::new(),
            },
            ZoneAnswer::StartOfAuthority => DnsResponseRecords {
                answer_records: vec![self.records.soa.clone()],
                soa_records: Vec::new(),
            },
            ZoneAnswer::PeerAddresses { peers } => {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "histogram sample of a small peer count"
                )]
                histogram!(DNS_RESPONSE_PEERS, LABEL_NETWORK => self.network.label())
                    .record(peers.len() as f64);

                let answer_records = peers
                    .iter()
                    .copied()
                    .map(|addr| {
                        let rdata = match addr.ip() {
                            IpAddr::V4(ipv4) => RData::A(hickory_proto::rr::rdata::A(ipv4)),
                            IpAddr::V6(ipv6) => RData::AAAA(hickory_proto::rr::rdata::AAAA(ipv6)),
                        };
                        Record::from_rdata(query_name.clone().into(), self.ttl, rdata)
                    })
                    .collect();

                DnsResponseRecords {
                    answer_records,
                    soa_records: Vec::new(),
                }
            }
        }
    }
}

fn parse_absolute_name(field: &str, name_text: &str) -> Result<Name> {
    let mut name =
        Name::from_ascii(name_text).wrap_err_with(|| format!("invalid {field} `{name_text}`"))?;
    name.set_fqdn(true);
    Ok(name)
}

impl ZoneRecords {
    fn new(seed_domain: Name, nameserver_name: Name, dns_ttl: u32) -> Result<Self> {
        let seed_domain_ascii = seed_domain.to_ascii();
        let responsible_mailbox = Name::from_ascii(format!("hostmaster.{seed_domain_ascii}"))
            .wrap_err_with(|| {
                format!("invalid synthesized SOA mailbox for `{seed_domain_ascii}`")
            })?;
        let soa = SOA::new(
            nameserver_name.clone(),
            responsible_mailbox,
            SOA_SERIAL,
            SOA_REFRESH_SECONDS,
            SOA_RETRY_SECONDS,
            SOA_EXPIRE_SECONDS,
            dns_ttl,
        );

        Ok(Self {
            soa: Record::from_rdata(seed_domain.clone(), dns_ttl, RData::SOA(soa)),
            nameserver: Record::from_rdata(seed_domain, dns_ttl, RData::NS(NS(nameserver_name))),
        })
    }
}

impl DnsRequestHandler {
    /// Build a DNS request handler that serves `zones` behind a shared limiter.
    pub(crate) fn new(zones: Vec<SeedZone>, rate_limiter: Option<RateLimiter>) -> Self {
        Self {
            zones,
            rate_limiter,
        }
    }

    /// The most specific zone authoritative for `name`, if any.
    ///
    /// Configuration rejects overlapping zones, so at most one zone matches;
    /// the longest-domain tie-break is a defensive belt-and-braces choice.
    fn matching_zone(&self, name: &LowerName) -> Option<&SeedZone> {
        self.zones
            .iter()
            .filter(|zone| zone.contains(name))
            .max_by_key(|zone| zone.domain.num_labels())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneAnswer<'a> {
    NoData,
    Nameserver,
    StartOfAuthority,
    PeerAddresses { peers: &'a [PeerSocketAddr] },
}

fn record_type_label(record_type: RecordType) -> &'static str {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "unsupported DNS record types share the stable 'other' metrics label"
    )]
    match record_type {
        RecordType::A => RECORD_TYPE_A,
        RecordType::AAAA => RECORD_TYPE_AAAA,
        RecordType::NS => RECORD_TYPE_NS,
        RecordType::SOA => RECORD_TYPE_SOA,
        _ => RECORD_TYPE_OTHER,
    }
}

fn resolve_peer_answer(peers: &[PeerSocketAddr]) -> ZoneAnswer<'_> {
    if peers.is_empty() {
        ZoneAnswer::NoData
    } else {
        ZoneAnswer::PeerAddresses { peers }
    }
}

#[derive(Default)]
struct DnsResponseRecords {
    answer_records: Vec<Record>,
    soa_records: Vec<Record>,
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
                counter!(DNS_RATE_LIMITED_TOTAL).increment(1);

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
        let response_records = if let Some(query) = request.queries.queries().first() {
            let name = query.name();
            let record_type = query.query_type();
            counter!(
                DNS_QUERIES_TOTAL,
                &[(LABEL_RECORD_TYPE, record_type_label(record_type))]
            )
            .increment(1);

            #[allow(
                clippy::option_if_let_else,
                reason = "the no-match arm mutates response metadata, so a match keeps that side effect visible"
            )]
            match self.matching_zone(name) {
                None => {
                    metadata.response_code = ResponseCode::Refused;
                    DnsResponseRecords::default()
                }
                Some(zone) => {
                    let servable_peers = zone.servable_peers.borrow().clone();
                    let answer = zone.resolve_answer(name, record_type, &servable_peers);
                    zone.response_records_for_answer(name, answer)
                }
            }
        } else {
            DnsResponseRecords::default()
        };

        let response = builder.build(
            metadata,
            response_records.answer_records.iter(),
            &[],
            response_records.soa_records.iter(),
            &[],
        );
        response_handle
            .send_response(response)
            .await
            .inspect_err(|e| {
                tracing::warn!("failed to send DNS response: {e}");
                counter!(DNS_ERRORS_TOTAL).increment(1);
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
        sync::{Arc, Mutex},
    };

    use hickory_proto::{
        op::{Message, MessageType, OpCode, Query},
        serialize::binary::BinEncoder,
    };
    use hickory_server::{
        net::{NetError, xfer::Protocol},
        server::Request,
        zone_handler::MessageResponse,
    };
    use zebra_network::PeerSocketAddr;

    use super::*;
    use crate::{config::RateLimitConfig, dns::rate_limiter::RateLimiter};

    type TestResult = color_eyre::Result<()>;

    #[derive(Clone, Default)]
    struct CapturingResponseHandler {
        response: Arc<Mutex<Option<Message>>>,
    }

    #[async_trait::async_trait]
    impl ResponseHandler for CapturingResponseHandler {
        async fn send_response<'a>(
            &mut self,
            response: MessageResponse<
                '_,
                'a,
                impl Iterator<Item = &'a Record> + Send + 'a,
                impl Iterator<Item = &'a Record> + Send + 'a,
                impl Iterator<Item = &'a Record> + Send + 'a,
                impl Iterator<Item = &'a Record> + Send + 'a,
            >,
        ) -> Result<ResponseInfo, NetError> {
            let mut bytes = Vec::new();
            let mut encoder = BinEncoder::new(&mut bytes);
            let response_info = response.destructive_emit(&mut encoder)?;
            let message = Message::from_vec(&bytes)?;
            {
                let mut captured_response = self
                    .response
                    .lock()
                    .map_err(|_| NetError::from("captured response mutex was poisoned"))?;
                *captured_response = Some(message);
            }

            Ok(response_info)
        }
    }

    fn request_for(query_name: &str, record_type: RecordType) -> color_eyre::Result<Request> {
        let mut message = Message::new(1, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(Name::from_ascii(query_name)?, record_type));
        let request_bytes = message.to_vec()?;
        Ok(Request::from_bytes(
            request_bytes,
            SocketAddr::from(([127, 0, 0, 1], 10_000)),
            Protocol::Udp,
        )?)
    }

    async fn answer_message(
        request_handler: &DnsRequestHandler,
        query_name: &str,
        record_type: RecordType,
    ) -> color_eyre::Result<Option<Message>> {
        let response_handler = CapturingResponseHandler::default();
        let captured_response = response_handler.response.clone();
        let request = request_for(query_name, record_type)?;

        request_handler
            .answer_request(&request, response_handler)
            .await;

        let response = {
            let mut response = captured_response
                .lock()
                .map_err(|_| color_eyre::eyre::eyre!("captured response mutex was poisoned"))?;
            response.take()
        };
        Ok(response)
    }

    async fn required_answer_message(
        request_handler: &DnsRequestHandler,
        query_name: &str,
        record_type: RecordType,
    ) -> color_eyre::Result<Message> {
        answer_message(request_handler, query_name, record_type)
            .await?
            .ok_or_else(|| color_eyre::eyre::eyre!("DNS handler did not send a response"))
    }

    fn zone(
        network: ZcashNetwork,
        seed_domain: &str,
        nameserver: &str,
        servable_peers: ServablePeers,
    ) -> color_eyre::Result<SeedZone> {
        let (_sender, receiver) = watch::channel(servable_peers);
        SeedZone::new(network, seed_domain, nameserver, 600, receiver)
    }

    fn single_zone_handler(
        seed_domain: &str,
        servable_peers: ServablePeers,
        rate_limiter: Option<RateLimiter>,
    ) -> color_eyre::Result<DnsRequestHandler> {
        let seed_zone = zone(
            ZcashNetwork::Mainnet,
            seed_domain,
            "ns.seeder.test",
            servable_peers,
        )?;
        Ok(DnsRequestHandler::new(vec![seed_zone], rate_limiter))
    }

    fn peer(ip: IpAddr) -> PeerSocketAddr {
        PeerSocketAddr::from(SocketAddr::new(ip, 8233))
    }

    fn servable_peer_snapshot() -> ServablePeers {
        ServablePeers {
            ipv4: vec![peer(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))].into(),
            ipv6: vec![peer(IpAddr::V6(Ipv6Addr::new(
                0x2001, 0x0db8, 0, 0, 0, 0, 0, 1,
            )))]
            .into(),
        }
    }

    fn lower_name(name: &str) -> color_eyre::Result<LowerName> {
        Ok(LowerName::from(Name::from_ascii(name)?))
    }

    /// Resolve an answer for a single-zone setup, mirroring the old per-zone
    /// unit-test seam now that routing lives in the handler.
    fn answer_for<'a>(
        seed_domain: &str,
        query_name: &str,
        record_type: RecordType,
        servable_peers: &'a ServablePeers,
    ) -> color_eyre::Result<ZoneAnswer<'a>> {
        let seed_zone = zone(
            ZcashNetwork::Mainnet,
            seed_domain,
            "ns.seeder.test",
            ServablePeers::default(),
        )?;
        Ok(seed_zone.resolve_answer(&lower_name(query_name)?, record_type, servable_peers))
    }

    #[test]
    fn answers_apex_queries_for_seed_domain() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        let answer = answer_for(
            "mainnet.seeder.test",
            "mainnet.seeder.test",
            RecordType::A,
            &servable_peers,
        )?;

        assert_eq!(
            answer,
            ZoneAnswer::PeerAddresses {
                peers: servable_peers.ipv4.as_ref(),
            }
        );
        Ok(())
    }

    #[test]
    fn subdomain_queries_under_seed_domain_return_no_data() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        let answer = answer_for(
            "mainnet.seeder.test",
            "node.mainnet.seeder.test",
            RecordType::AAAA,
            &servable_peers,
        )?;

        assert_eq!(answer, ZoneAnswer::NoData);
        Ok(())
    }

    #[test]
    fn answers_trailing_dot_queries_for_seed_domain() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        let answer = answer_for(
            "mainnet.seeder.test",
            "mainnet.seeder.test.",
            RecordType::A,
            &servable_peers,
        )?;

        assert_eq!(
            answer,
            ZoneAnswer::PeerAddresses {
                peers: servable_peers.ipv4.as_ref(),
            }
        );
        Ok(())
    }

    #[test]
    fn answers_mixed_case_seed_domain_queries() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        let answer = answer_for(
            "MainNet.Seeder.Test",
            "mainnet.seeder.test.",
            RecordType::A,
            &servable_peers,
        )?;

        assert_eq!(
            answer,
            ZoneAnswer::PeerAddresses {
                peers: servable_peers.ipv4.as_ref(),
            }
        );
        Ok(())
    }

    #[test]
    fn answers_aaaa_queries_with_ipv6_peer_family() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        let answer = answer_for(
            "mainnet.seeder.test",
            "mainnet.seeder.test",
            RecordType::AAAA,
            &servable_peers,
        )?;

        assert_eq!(
            answer,
            ZoneAnswer::PeerAddresses {
                peers: servable_peers.ipv6.as_ref(),
            }
        );
        Ok(())
    }

    #[test]
    fn answers_nameserver_and_start_of_authority_for_seed_domain() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "mainnet.seeder.test",
                RecordType::NS,
                &servable_peers,
            )?,
            ZoneAnswer::Nameserver
        );
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "mainnet.seeder.test",
                RecordType::SOA,
                &servable_peers,
            )?,
            ZoneAnswer::StartOfAuthority
        );
        Ok(())
    }

    #[test]
    fn returns_no_data_for_unsupported_record_types() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "mainnet.seeder.test",
                RecordType::TXT,
                &servable_peers,
            )?,
            ZoneAnswer::NoData
        );
        Ok(())
    }

    #[test]
    fn empty_apex_peer_families_return_no_data() -> TestResult {
        let servable_peers = ServablePeers::default();

        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "mainnet.seeder.test",
                RecordType::A,
                &servable_peers,
            )?,
            ZoneAnswer::NoData
        );
        Ok(())
    }

    #[test]
    fn unsupported_record_types_use_other_metric_label() {
        assert_eq!(record_type_label(RecordType::A), RECORD_TYPE_A);
        assert_eq!(record_type_label(RecordType::AAAA), RECORD_TYPE_AAAA);
        assert_eq!(record_type_label(RecordType::SOA), RECORD_TYPE_SOA);
        assert_eq!(record_type_label(RecordType::NS), RECORD_TYPE_NS);
        assert_eq!(record_type_label(RecordType::TXT), RECORD_TYPE_OTHER);
    }

    #[test]
    fn rejects_invalid_seed_domain() {
        let result = zone(
            ZcashNetwork::Mainnet,
            "not a domain",
            "ns.seeder.test",
            ServablePeers::default(),
        );

        assert!(result.is_err(), "invalid seed domain should fail startup");
    }

    #[test]
    fn rejects_in_zone_nameserver() {
        let result = zone(
            ZcashNetwork::Mainnet,
            "mainnet.seeder.test",
            "ns.mainnet.seeder.test",
            ServablePeers::default(),
        );

        assert!(
            result.is_err(),
            "in-zone nameserver should fail because Zeeder does not serve glue"
        );
    }

    #[tokio::test]
    async fn refuses_queries_outside_every_zone() -> TestResult {
        let handler = single_zone_handler("mainnet.seeder.test", ServablePeers::default(), None)?;

        for outside in ["wrong.domain.test", "evilmainnet.seeder.test"] {
            let response = required_answer_message(&handler, outside, RecordType::A).await?;
            assert_eq!(
                response.metadata.response_code,
                ResponseCode::Refused,
                "{outside} is outside every zone and must be REFUSED"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn routes_each_query_to_its_own_network_zone() -> TestResult {
        let mainnet_v4 = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        let testnet_v4 = IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2));

        let mainnet_zone = zone(
            ZcashNetwork::Mainnet,
            "mainnet.seeder.test",
            "ns-mainnet.seeder.test",
            ServablePeers {
                ipv4: vec![peer(mainnet_v4)].into(),
                ipv6: Arc::default(),
            },
        )?;
        let testnet_zone = zone(
            ZcashNetwork::Testnet,
            "testnet.seeder.test",
            "ns-testnet.seeder.test",
            ServablePeers {
                ipv4: vec![peer(testnet_v4)].into(),
                ipv6: Arc::default(),
            },
        )?;
        let handler = DnsRequestHandler::new(vec![mainnet_zone, testnet_zone], None);

        let mainnet =
            required_answer_message(&handler, "mainnet.seeder.test", RecordType::A).await?;
        let mainnet_ips: HashSet<IpAddr> = mainnet
            .answers
            .iter()
            .filter_map(|record| record.data.ip_addr())
            .collect();
        assert_eq!(
            mainnet_ips,
            HashSet::from([mainnet_v4]),
            "the mainnet zone must answer only mainnet peers"
        );

        let testnet =
            required_answer_message(&handler, "testnet.seeder.test", RecordType::A).await?;
        let testnet_ips: HashSet<IpAddr> = testnet
            .answers
            .iter()
            .filter_map(|record| record.data.ip_addr())
            .collect();
        assert_eq!(
            testnet_ips,
            HashSet::from([testnet_v4]),
            "the testnet zone must answer only testnet peers"
        );

        let other = required_answer_message(&handler, "other.example.com", RecordType::A).await?;
        assert_eq!(
            other.metadata.response_code,
            ResponseCode::Refused,
            "a name in neither zone must be REFUSED"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rate_limits_excessive_queries_without_sending_response() -> TestResult {
        let rate_limiter = RateLimiter::new(&RateLimitConfig {
            queries_per_second: 1,
            burst_size: 2,
        })?;
        let handler = single_zone_handler(
            "mainnet.seeder.test",
            ServablePeers::default(),
            Some(rate_limiter),
        )?;

        assert!(
            answer_message(&handler, "mainnet.seeder.test", RecordType::A)
                .await?
                .is_some(),
            "first query should be answered"
        );
        assert!(
            answer_message(&handler, "mainnet.seeder.test", RecordType::A)
                .await?
                .is_some(),
            "second query should be answered"
        );
        assert!(
            answer_message(&handler, "mainnet.seeder.test", RecordType::A)
                .await?
                .is_none(),
            "third query should be silently dropped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn sends_cached_servable_peers_and_zone_metadata() -> TestResult {
        let v4a = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        let v4b = IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8));
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));

        let servable_peers = ServablePeers {
            ipv4: vec![peer(v4a), peer(v4b)].into(),
            ipv6: vec![peer(v6)].into(),
        };

        let handler = single_zone_handler("mainnet.seeder.test", servable_peers, None)?;
        let a = required_answer_message(&handler, "mainnet.seeder.test", RecordType::A).await?;
        assert!(
            a.metadata.authoritative,
            "A response should be authoritative"
        );
        assert_eq!(a.metadata.response_code, ResponseCode::NoError);
        let got_v4: HashSet<IpAddr> = a
            .answers
            .iter()
            .filter_map(|record| record.data.ip_addr())
            .collect();
        assert_eq!(
            got_v4,
            HashSet::from([v4a, v4b]),
            "A query should return exactly the cached IPv4 peers"
        );

        let aaaa =
            required_answer_message(&handler, "mainnet.seeder.test", RecordType::AAAA).await?;
        let got_v6: HashSet<IpAddr> = aaaa
            .answers
            .iter()
            .filter_map(|record| record.data.ip_addr())
            .collect();
        assert_eq!(
            got_v6,
            HashSet::from([v6]),
            "AAAA query should return exactly the cached IPv6 peers"
        );

        let soa = required_answer_message(&handler, "mainnet.seeder.test", RecordType::SOA).await?;
        assert!(
            soa.answers
                .iter()
                .any(|record| matches!(&record.data, RData::SOA(_))),
            "SOA query should return an SOA record"
        );

        let ns = required_answer_message(&handler, "mainnet.seeder.test", RecordType::NS).await?;
        assert_eq!(ns.answers.len(), 1, "NS query should return one record");
        let expected_nameserver = LowerName::from(Name::from_ascii("ns.seeder.test.")?);
        let RData::NS(ns) = &ns.answers[0].data else {
            return Err(color_eyre::eyre::eyre!("expected NS record"));
        };
        assert_eq!(LowerName::from(ns.0.clone()), expected_nameserver);

        let no_data =
            required_answer_message(&handler, "node.mainnet.seeder.test", RecordType::A).await?;
        assert!(
            no_data.answers.is_empty(),
            "in-zone subdomain should not return peer records"
        );
        assert!(
            no_data
                .authorities
                .iter()
                .any(|record| matches!(&record.data, RData::SOA(_))),
            "in-zone subdomain should include SOA authority"
        );

        Ok(())
    }

    #[tokio::test]
    async fn empty_exact_name_peer_family_includes_soa_authority() -> TestResult {
        let handler = single_zone_handler("mainnet.seeder.test", ServablePeers::default(), None)?;

        let response =
            required_answer_message(&handler, "mainnet.seeder.test", RecordType::AAAA).await?;

        assert!(
            response.answers.is_empty(),
            "empty peer family should not return address records"
        );
        assert!(
            response
                .authorities
                .iter()
                .any(|record| matches!(&record.data, RData::SOA(_))),
            "empty peer family should include SOA authority"
        );
        Ok(())
    }
}
