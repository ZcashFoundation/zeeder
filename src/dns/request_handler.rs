//! DNS request handling backed by the latest servable peer snapshot.

use std::net::IpAddr;

use color_eyre::eyre::{Context, Result};
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
    crawl::address_cache::ServablePeers,
    dns::rate_limiter::RateLimiter,
    metrics::{
        DNS_ERRORS_TOTAL, DNS_QUERIES_TOTAL, DNS_RATE_LIMITED_TOTAL, DNS_RESPONSE_PEERS,
        LABEL_RECORD_TYPE, RECORD_TYPE_A, RECORD_TYPE_AAAA, RECORD_TYPE_NS, RECORD_TYPE_SOA,
    },
};

const SOA_SERIAL: u32 = 1;
const SOA_REFRESH_SECONDS: i32 = 3_600;
const SOA_RETRY_SECONDS: i32 = 600;
const SOA_EXPIRE_SECONDS: i32 = 86_400;

/// Hickory request handler for the seed domain.
#[derive(Clone)]
pub(crate) struct DnsRequestHandler {
    servable_peers: watch::Receiver<ServablePeers>,
    seed_zone: SeedZone,
    rate_limiter: Option<RateLimiter>,
}

/// Parsed DNS zone served by the seeder.
#[derive(Clone)]
pub(crate) struct SeedZone {
    domain: LowerName,
    records: ZoneRecords,
    ttl: u32,
}

#[derive(Clone)]
struct ZoneRecords {
    soa: Record,
    nameserver: Record,
}

impl SeedZone {
    /// Parse the configured seed domain and synthesize static zone metadata.
    pub(crate) fn new(seed_domain: &str, dns_ttl: u32) -> Result<Self> {
        let seed_domain = Name::from_ascii(seed_domain)
            .wrap_err_with(|| format!("invalid seed domain `{seed_domain}`"))?;
        let records = ZoneRecords::new(seed_domain.clone(), dns_ttl)?;

        Ok(Self {
            domain: LowerName::from(seed_domain),
            records,
            ttl: dns_ttl,
        })
    }

    fn resolve_answer<'a>(
        &self,
        query_name: &LowerName,
        record_type: RecordType,
        servable_peers: &'a ServablePeers,
    ) -> DnsAnswer<'a> {
        if !self.domain.zone_of(query_name) {
            return DnsAnswer::Refused;
        }
        if query_name.num_labels() != self.domain.num_labels() {
            return DnsAnswer::NoData;
        }

        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "RecordType has many variants; the seeder serves only A and AAAA"
        )]
        match record_type {
            RecordType::A => resolve_peer_answer(RECORD_TYPE_A, &servable_peers.ipv4),
            RecordType::AAAA => resolve_peer_answer(RECORD_TYPE_AAAA, &servable_peers.ipv6),
            RecordType::NS => DnsAnswer::Nameserver,
            RecordType::SOA => DnsAnswer::StartOfAuthority,
            _ => DnsAnswer::NoData,
        }
    }
}

impl ZoneRecords {
    fn new(seed_domain: Name, dns_ttl: u32) -> Result<Self> {
        let seed_domain_ascii = seed_domain.to_ascii();
        let nameserver_name =
            Name::from_ascii(format!("ns.{seed_domain_ascii}")).wrap_err_with(|| {
                format!("invalid synthesized nameserver for `{seed_domain_ascii}`")
            })?;
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
    /// Build a DNS request handler using the latest servable peer snapshot.
    pub(crate) fn new(
        servable_peers: watch::Receiver<ServablePeers>,
        seed_zone: SeedZone,
        rate_limiter: Option<RateLimiter>,
    ) -> Self {
        Self {
            servable_peers,
            seed_zone,
            rate_limiter,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DnsAnswer<'a> {
    Refused,
    NoData,
    Nameserver,
    StartOfAuthority,
    PeerAddresses {
        record_type_label: &'static str,
        peers: &'a [PeerSocketAddr],
    },
}

fn resolve_peer_answer<'a>(
    record_type_label: &'static str,
    peers: &'a [PeerSocketAddr],
) -> DnsAnswer<'a> {
    if peers.is_empty() {
        DnsAnswer::NoData
    } else {
        DnsAnswer::PeerAddresses {
            record_type_label,
            peers,
        }
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

            {
                let servable_peers = self.servable_peers.borrow().clone();
                let answer = self
                    .seed_zone
                    .resolve_answer(name, record_type, &servable_peers);
                self.response_records_for_answer(name, answer, &mut metadata)
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

    fn response_records_for_answer(
        &self,
        query_name: &LowerName,
        answer: DnsAnswer<'_>,
        metadata: &mut Metadata,
    ) -> DnsResponseRecords {
        match answer {
            DnsAnswer::Refused => {
                metadata.response_code = ResponseCode::Refused;
                DnsResponseRecords::default()
            }
            DnsAnswer::NoData => DnsResponseRecords {
                answer_records: Vec::new(),
                soa_records: vec![self.seed_zone.records.soa.clone()],
            },
            DnsAnswer::Nameserver => {
                counter!(DNS_QUERIES_TOTAL, &[(LABEL_RECORD_TYPE, RECORD_TYPE_NS)]).increment(1);
                DnsResponseRecords {
                    answer_records: vec![self.seed_zone.records.nameserver.clone()],
                    soa_records: Vec::new(),
                }
            }
            DnsAnswer::StartOfAuthority => {
                counter!(DNS_QUERIES_TOTAL, &[(LABEL_RECORD_TYPE, RECORD_TYPE_SOA)]).increment(1);
                DnsResponseRecords {
                    answer_records: vec![self.seed_zone.records.soa.clone()],
                    soa_records: Vec::new(),
                }
            }
            DnsAnswer::PeerAddresses {
                record_type_label,
                peers,
            } => {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "histogram sample of a small peer count"
                )]
                histogram!(DNS_RESPONSE_PEERS).record(peers.len() as f64);
                counter!(DNS_QUERIES_TOTAL, &[(LABEL_RECORD_TYPE, record_type_label)]).increment(1);

                let answer_records = peers
                    .iter()
                    .copied()
                    .map(|addr| {
                        let rdata = match addr.ip() {
                            IpAddr::V4(ipv4) => RData::A(hickory_proto::rr::rdata::A(ipv4)),
                            IpAddr::V6(ipv6) => RData::AAAA(hickory_proto::rr::rdata::AAAA(ipv6)),
                        };
                        Record::from_rdata(query_name.clone().into(), self.seed_zone.ttl, rdata)
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

    fn servable_peers_receiver(servable_peers: ServablePeers) -> watch::Receiver<ServablePeers> {
        let (_sender, receiver) = watch::channel(servable_peers);
        receiver
    }

    fn seed_zone(seed_domain: &str, dns_ttl: u32) -> color_eyre::Result<SeedZone> {
        SeedZone::new(seed_domain, dns_ttl)
    }

    fn request_handler(
        servable_peers: ServablePeers,
        seed_domain: &str,
        dns_ttl: u32,
        rate_limiter: Option<RateLimiter>,
    ) -> color_eyre::Result<DnsRequestHandler> {
        Ok(DnsRequestHandler::new(
            servable_peers_receiver(servable_peers),
            seed_zone(seed_domain, dns_ttl)?,
            rate_limiter,
        ))
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

    fn answer_for<'a>(
        seed_domain: &str,
        query_name: &str,
        record_type: RecordType,
        servable_peers: &'a ServablePeers,
    ) -> color_eyre::Result<DnsAnswer<'a>> {
        let seed_zone = seed_zone(seed_domain, 600)?;
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
            DnsAnswer::PeerAddresses {
                record_type_label: RECORD_TYPE_A,
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

        assert_eq!(answer, DnsAnswer::NoData);
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
            DnsAnswer::PeerAddresses {
                record_type_label: RECORD_TYPE_A,
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
            DnsAnswer::PeerAddresses {
                record_type_label: RECORD_TYPE_A,
                peers: servable_peers.ipv4.as_ref(),
            }
        );
        Ok(())
    }

    #[test]
    fn refuses_queries_outside_seed_domain() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "wrong.domain.test",
                RecordType::A,
                &servable_peers,
            )?,
            DnsAnswer::Refused
        );
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "evilmainnet.seeder.test",
                RecordType::A,
                &servable_peers,
            )?,
            DnsAnswer::Refused
        );
        Ok(())
    }

    #[test]
    fn answers_nameserver_queries_for_seed_domain() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "mainnet.seeder.test",
                RecordType::NS,
                &servable_peers,
            )?,
            DnsAnswer::Nameserver
        );
        Ok(())
    }

    #[test]
    fn answers_start_of_authority_queries_for_seed_domain() -> TestResult {
        let servable_peers = servable_peer_snapshot();
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "mainnet.seeder.test",
                RecordType::SOA,
                &servable_peers,
            )?,
            DnsAnswer::StartOfAuthority
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
            DnsAnswer::NoData
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
            DnsAnswer::PeerAddresses {
                record_type_label: RECORD_TYPE_AAAA,
                peers: servable_peers.ipv6.as_ref(),
            }
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
            DnsAnswer::NoData
        );
        assert_eq!(
            answer_for(
                "mainnet.seeder.test",
                "mainnet.seeder.test",
                RecordType::AAAA,
                &servable_peers,
            )?,
            DnsAnswer::NoData
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_seed_domain() {
        let result = SeedZone::new("not a domain", 600);

        assert!(result.is_err(), "invalid seed domain should fail startup");
    }

    #[test]
    fn synthesizes_zone_records_for_seed_domain() -> TestResult {
        let seed_zone = SeedZone::new("mainnet.seeder.test", 300)?;

        assert_eq!(seed_zone.records.soa.name.to_ascii(), "mainnet.seeder.test");
        assert_eq!(
            seed_zone.records.nameserver.name.to_ascii(),
            "mainnet.seeder.test"
        );
        assert_eq!(seed_zone.records.soa.ttl, 300);
        assert_eq!(seed_zone.records.nameserver.ttl, 300);
        assert!(matches!(&seed_zone.records.soa.data, RData::SOA(_)));
        assert!(matches!(&seed_zone.records.nameserver.data, RData::NS(_)));
        Ok(())
    }

    #[test]
    fn refused_answers_set_refused_response_code() -> TestResult {
        let handler = request_handler(ServablePeers::default(), "mainnet.seeder.test", 600, None)?;
        let mut metadata = Metadata::new(0, MessageType::Response, OpCode::Query);

        let response_records = handler.response_records_for_answer(
            &lower_name("wrong.domain.test")?,
            DnsAnswer::Refused,
            &mut metadata,
        );

        assert_eq!(metadata.response_code, ResponseCode::Refused);
        assert!(response_records.answer_records.is_empty());
        assert!(response_records.soa_records.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn rate_limits_excessive_queries_without_sending_response() -> TestResult {
        let rate_limiter = RateLimiter::new(&RateLimitConfig {
            queries_per_second: 1,
            burst_size: 2,
        })?;
        let request_handler = request_handler(
            ServablePeers::default(),
            "mainnet.seeder.test",
            600,
            Some(rate_limiter),
        )?;

        assert!(
            answer_message(&request_handler, "mainnet.seeder.test", RecordType::A)
                .await?
                .is_some(),
            "first query should be answered"
        );
        assert!(
            answer_message(&request_handler, "mainnet.seeder.test", RecordType::A)
                .await?
                .is_some(),
            "second query should be answered"
        );
        assert!(
            answer_message(&request_handler, "mainnet.seeder.test", RecordType::A)
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

        let request_handler = request_handler(servable_peers, "mainnet.seeder.test", 600, None)?;
        let a =
            required_answer_message(&request_handler, "mainnet.seeder.test", RecordType::A).await?;
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
            required_answer_message(&request_handler, "mainnet.seeder.test", RecordType::AAAA)
                .await?;
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

        let soa = required_answer_message(&request_handler, "mainnet.seeder.test", RecordType::SOA)
            .await?;
        assert!(
            soa.answers
                .iter()
                .any(|record| matches!(&record.data, RData::SOA(_))),
            "SOA query should return an SOA record"
        );

        let ns = required_answer_message(&request_handler, "mainnet.seeder.test", RecordType::NS)
            .await?;
        assert!(
            ns.answers
                .iter()
                .any(|record| matches!(&record.data, RData::NS(_))),
            "NS query should return an NS record"
        );

        let no_data =
            required_answer_message(&request_handler, "node.mainnet.seeder.test", RecordType::A)
                .await?;
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
        let servable_peers = ServablePeers {
            ipv4: Vec::new().into(),
            ipv6: Vec::new().into(),
        };
        let request_handler = request_handler(servable_peers, "mainnet.seeder.test", 600, None)?;

        let response =
            required_answer_message(&request_handler, "mainnet.seeder.test", RecordType::AAAA)
                .await?;

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
