//! Prometheus metrics endpoint.

use color_eyre::eyre::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;

pub(crate) const PEERS_KNOWN: &str = "seeder_peers_known";
pub(crate) const PEERS_SERVABLE: &str = "seeder_peers_servable";
pub(crate) const PEERS_UNSERVABLE: &str = "seeder_peers_unservable";
pub(crate) const MIN_PROTOCOL_VERSION: &str = "seeder_min_protocol_version";
pub(crate) const BUILD_INFO: &str = "seeder_build_info";
pub(crate) const MUTEX_POISONING_TOTAL: &str = "seeder_mutex_poisoning_total";
pub(crate) const DNS_RATE_LIMITED_TOTAL: &str = "seeder_dns_rate_limited_total";
pub(crate) const DNS_ERRORS_TOTAL: &str = "seeder_dns_errors_total";
pub(crate) const DNS_QUERIES_TOTAL: &str = "seeder_dns_queries_total";
pub(crate) const DNS_RESPONSE_PEERS: &str = "seeder_dns_response_peers";

pub(crate) const LABEL_ADDR_FAMILY: &str = "addr_family";
pub(crate) const LABEL_REASON: &str = "reason";
pub(crate) const LABEL_RECORD_TYPE: &str = "record_type";
pub(crate) const LABEL_VERSION: &str = "version";
pub(crate) const LABEL_GIT_SHA: &str = "git_sha";
pub(crate) const LABEL_NETWORK: &str = "network";

pub(crate) const ADDR_FAMILY_IPV4: &str = "v4";
pub(crate) const ADDR_FAMILY_IPV6: &str = "v6";

pub(crate) const RECORD_TYPE_A: &str = "A";
pub(crate) const RECORD_TYPE_AAAA: &str = "AAAA";
pub(crate) const RECORD_TYPE_SOA: &str = "SOA";
pub(crate) const RECORD_TYPE_NS: &str = "NS";

/// Install the Prometheus recorder and serve metrics at `addr`.
pub(crate) fn init(addr: SocketAddr) -> Result<()> {
    let builder = PrometheusBuilder::new();
    builder
        .with_http_listener(addr)
        .install()
        .wrap_err_with(|| {
            format!("failed to install Prometheus recorder for http://{addr}/metrics")
        })?;

    tracing::info!("Metrics endpoints listening on http://{addr}/metrics");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_docs_include_metric_contract_constants() {
        let operations_docs = include_str!("../docs/operations.md");
        let documented_terms = [
            PEERS_KNOWN,
            PEERS_SERVABLE,
            PEERS_UNSERVABLE,
            MIN_PROTOCOL_VERSION,
            BUILD_INFO,
            MUTEX_POISONING_TOTAL,
            DNS_RATE_LIMITED_TOTAL,
            DNS_ERRORS_TOTAL,
            DNS_QUERIES_TOTAL,
            DNS_RESPONSE_PEERS,
            LABEL_ADDR_FAMILY,
            LABEL_REASON,
            LABEL_RECORD_TYPE,
            LABEL_VERSION,
            LABEL_GIT_SHA,
            LABEL_NETWORK,
            ADDR_FAMILY_IPV4,
            ADDR_FAMILY_IPV6,
            RECORD_TYPE_A,
            RECORD_TYPE_AAAA,
            RECORD_TYPE_SOA,
            RECORD_TYPE_NS,
        ];

        for term in documented_terms {
            assert!(
                operations_docs.contains(term),
                "`{term}` should be documented in docs/operations.md"
            );
        }
    }
}
