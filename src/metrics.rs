//! Prometheus metrics endpoint.

use color_eyre::eyre::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;

pub(crate) const PEERS_KNOWN: &str = "zeeder_peers_known";
pub(crate) const PEERS_SERVABLE: &str = "zeeder_peers_servable";
pub(crate) const PEERS_UNSERVABLE: &str = "zeeder_peers_unservable";
pub(crate) const MIN_PROTOCOL_VERSION: &str = "zeeder_min_protocol_version";
pub(crate) const BUILD_INFO: &str = "zeeder_build_info";
pub(crate) const MUTEX_POISONING_TOTAL: &str = "zeeder_mutex_poisoning_total";
pub(crate) const DNS_RATE_LIMITED_TOTAL: &str = "zeeder_dns_rate_limited_total";
pub(crate) const DNS_ERRORS_TOTAL: &str = "zeeder_dns_errors_total";
pub(crate) const DNS_QUERIES_TOTAL: &str = "zeeder_dns_queries_total";
pub(crate) const DNS_RESPONSE_PEERS: &str = "zeeder_dns_response_peers";

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
pub(crate) const RECORD_TYPE_OTHER: &str = "other";

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
            RECORD_TYPE_OTHER,
        ];

        for term in documented_terms {
            assert!(
                operations_docs.contains(term),
                "`{term}` should be documented in docs/operations.md"
            );
        }
    }

    #[test]
    fn operations_docs_describe_peer_response_metric_as_prometheus_summary() {
        let operations_docs = include_str!("../docs/operations.md");
        let response_peers_metric_row = format!(
            "| `{DNS_RESPONSE_PEERS}` | Summary | `network=mainnet\\|testnet` | Peers per response | - |"
        );

        assert!(
            operations_docs.contains(&response_peers_metric_row),
            "`{DNS_RESPONSE_PEERS}` should be documented with its exported Prometheus type"
        );
    }

    #[test]
    fn operations_docs_use_servability_vocabulary_for_peer_metrics() {
        let operations_docs = include_str!("../docs/operations.md");
        let unservable_metric_row = format!(
            "| `{PEERS_UNSERVABLE}` | Gauge | `network=mainnet\\|testnet`, `reason=not_routable\\|wrong_port\\|not_recently_live\\|outdated_version\\|not_full_node\\|inbound\\|misbehaving` | Unservable peers, by reason | - |"
        );

        assert!(
            operations_docs.contains(&unservable_metric_row),
            "`{PEERS_UNSERVABLE}` should use the canonical unservable vocabulary"
        );
    }

    #[test]
    fn operations_docs_troubleshooting_uses_canonical_metric_names() {
        let operations_docs = include_str!("../docs/operations.md");

        for metric_name in [PEERS_SERVABLE, PEERS_KNOWN] {
            let troubleshooting_command =
                format!("curl -s http://localhost:9999/metrics | grep '{metric_name}'");

            assert!(
                operations_docs.contains(&troubleshooting_command),
                "`{metric_name}` troubleshooting command should use the full metric name"
            );
        }
    }

    #[test]
    fn operations_docs_describe_liveness_and_readiness_probes() {
        let operations_docs = include_str!("../docs/operations.md");

        assert!(
            operations_docs.contains("dig @127.0.0.1 -p 1053 testnet.seeder.example.com SOA"),
            "DNS liveness probe should be documented"
        );
        assert!(
            operations_docs.contains("GET /health"),
            "the liveness endpoint should be documented"
        );
        assert!(
            operations_docs.contains("GET /ready"),
            "the readiness endpoint should be documented"
        );
        assert!(
            operations_docs.contains("ready_threshold"),
            "readiness semantics should reference the per-zone peer threshold"
        );
    }
}
