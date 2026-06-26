//! Minimal health and readiness endpoint.
//!
//! `/health` reports process liveness. `/ready` reports `200` only when every
//! configured zone has at least `ready_threshold` servable peers, otherwise
//! `503` with a per-zone breakdown. The contract is a plain `200`/`503`, so a
//! hand-rolled HTTP/1.1 responder is used rather than a web framework.

use std::{fmt::Write as _, net::SocketAddr, sync::Arc};

use color_eyre::eyre::{Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};

use crate::{config::ZcashNetwork, crawl::address_cache::ServablePeers};

/// A network paired with its live servable-peer feed, used to compute readiness.
type ZoneReadiness = (ZcashNetwork, watch::Receiver<ServablePeers>);

/// The most request bytes the responder reads before answering. A health probe
/// request line and headers fit well within this.
const MAX_REQUEST_BYTES: usize = 1024;

/// Bind the health endpoint and spawn its accept loop.
pub(crate) async fn spawn(
    endpoint_addr: SocketAddr,
    ready_threshold: usize,
    zones: Vec<ZoneReadiness>,
) -> Result<()> {
    let listener = TcpListener::bind(endpoint_addr)
        .await
        .wrap_err_with(|| format!("failed to bind health endpoint on {endpoint_addr}"))?;

    tracing::info!("Health endpoint listening on http://{endpoint_addr}/health");

    let zones = Arc::new(zones);

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let zones = Arc::clone(&zones);
                    tokio::spawn(async move {
                        if let Err(error) = serve_connection(stream, &zones, ready_threshold).await
                        {
                            tracing::debug!("health connection error: {error}");
                        }
                    });
                }
                Err(error) => tracing::warn!("health endpoint accept error: {error}"),
            }
        }
    });

    Ok(())
}

async fn serve_connection(
    mut stream: TcpStream,
    zones: &[ZoneReadiness],
    ready_threshold: usize,
) -> std::io::Result<()> {
    let mut buffer = [0u8; MAX_REQUEST_BYTES];
    let read = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]);

    let response = match parse_request_path(&request) {
        Some("/health") => http_response(200, "OK", "ok\n"),
        Some("/ready" | "/") => readiness_response(zones, ready_threshold),
        _ => http_response(404, "Not Found", "not found\n"),
    };

    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Extract the request-target path from an HTTP request's first line, dropping
/// any query string.
fn parse_request_path(request: &str) -> Option<&str> {
    let target = request.lines().next()?.split_whitespace().nth(1)?;
    Some(target.split('?').next().unwrap_or(target))
}

fn readiness_response(zones: &[ZoneReadiness], ready_threshold: usize) -> String {
    let mut all_ready = true;
    let mut body = String::new();

    for (network, servable_peers) in zones {
        let servable = servable_peers.borrow().total();
        let ready = servable >= ready_threshold;
        all_ready = all_ready && ready;

        let state = if ready { "ready" } else { "not ready" };
        let _ = writeln!(
            body,
            "{}: {servable} servable peers ({state})",
            network.label()
        );
    }

    if all_ready {
        http_response(200, "OK", &body)
    } else {
        http_response(503, "Service Unavailable", &body)
    }
}

fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use zebra_network::PeerSocketAddr;

    use super::*;

    fn peer() -> PeerSocketAddr {
        PeerSocketAddr::from(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8233))
    }

    fn zone(network: ZcashNetwork, servable_peers: ServablePeers) -> ZoneReadiness {
        let (_sender, receiver) = watch::channel(servable_peers);
        (network, receiver)
    }

    #[test]
    fn parses_request_path_dropping_query() {
        assert_eq!(
            parse_request_path("GET /ready HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/ready")
        );
        assert_eq!(
            parse_request_path("GET /ready?probe=1 HTTP/1.1\r\n"),
            Some("/ready")
        );
        assert_eq!(parse_request_path(""), None);
    }

    #[test]
    fn ready_when_every_zone_meets_threshold() {
        let zones = vec![
            zone(
                ZcashNetwork::Mainnet,
                ServablePeers {
                    ipv4: vec![peer()].into(),
                    ipv6: Arc::default(),
                },
            ),
            zone(
                ZcashNetwork::Testnet,
                ServablePeers {
                    ipv4: vec![peer()].into(),
                    ipv6: Arc::default(),
                },
            ),
        ];

        let response = readiness_response(&zones, 1);
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("mainnet: 1 servable peers (ready)"));
        assert!(response.contains("testnet: 1 servable peers (ready)"));
    }

    #[test]
    fn not_ready_when_a_zone_is_below_threshold() {
        let zones = vec![
            zone(
                ZcashNetwork::Mainnet,
                ServablePeers {
                    ipv4: vec![peer()].into(),
                    ipv6: Arc::default(),
                },
            ),
            zone(ZcashNetwork::Testnet, ServablePeers::default()),
        ];

        let response = readiness_response(&zones, 1);
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(response.contains("testnet: 0 servable peers (not ready)"));
    }

    #[test]
    fn http_response_sets_content_length_to_body_bytes() {
        let response = http_response(200, "OK", "ok\n");
        assert!(response.contains("Content-Length: 3\r\n"));
        assert!(response.ends_with("ok\n"));
    }
}
