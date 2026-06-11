//! Prometheus metrics endpoint.

use color_eyre::eyre::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;

/// Install the Prometheus recorder and serve metrics at `addr`.
pub(crate) fn init(addr: SocketAddr) -> Result<()> {
    let builder = PrometheusBuilder::new();
    builder
        .with_http_listener(addr)
        .install()
        .wrap_err("failed to install Prometheus recorder")?;

    tracing::info!("Metrics endpoints listening on http://{addr}/metrics");

    Ok(())
}
