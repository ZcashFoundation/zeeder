//! Process composition for zebra-network crawling and DNS serving.

use std::{net::SocketAddr, time::Duration};

use color_eyre::eyre::{Context, Result};
use hickory_server::server::Server;
use metrics::gauge;
use tokio::net::{TcpListener, UdpSocket};
use zebra_chain::chain_tip::ChainTip;

use crate::{
    build_info,
    config::SeederConfig,
    crawl::{address_cache, chain_tip},
    dns::{
        rate_limiter::RateLimiter,
        request_handler::{DnsRequestHandler, SeedZone},
    },
    metrics::{BUILD_INFO, LABEL_GIT_SHA, LABEL_NETWORK, LABEL_VERSION, MIN_PROTOCOL_VERSION},
};

struct DnsSockets {
    udp_socket: UdpSocket,
    tcp_listener: TcpListener,
}

/// Run the seeder until the DNS server exits or the process receives a shutdown signal.
pub(crate) async fn run(config: SeederConfig) -> Result<()> {
    let seed_zone = SeedZone::new(&config.dns.domain, config.dns.ttl)?;
    let dns_sockets = bind_dns_sockets(config.dns.listen_addr).await?;

    tracing::info!("Initializing zebra-network...");
    let network_config = config.crawler.network_config();
    let network = network_config.network.clone();

    // Dummy inbound service that rejects everything.
    let inbound_service = tower::service_fn(|_req: zebra_network::Request| async move {
        Ok::<zebra_network::Response, Box<dyn std::error::Error + Send + Sync + 'static>>(
            zebra_network::Response::Nil,
        )
    });

    let user_agent = build_info::user_agent();

    tracing::info!("User-Agent: {user_agent}");

    // Pin a chain tip at the current network upgrade so zebra-network's
    // handshake rejects peers advertising an outdated protocol version.
    let tip = chain_tip::SeederChainTip::at_current_upgrade(&network);
    let min_protocol_version =
        zebra_network::Version::min_remote_for_height(&network, tip.best_tip_height());
    tracing::info!(
        network = %network,
        %min_protocol_version,
        "enforcing peer protocol-version floor"
    );
    gauge!(MIN_PROTOCOL_VERSION).set(f64::from(min_protocol_version.0));
    gauge!(
        BUILD_INFO,
        LABEL_VERSION => build_info::VERSION,
        LABEL_GIT_SHA => build_info::git_sha_label(),
        LABEL_NETWORK => network.to_string(),
    )
    .set(1.0);

    let (peer_set, address_book, _misbehavior_sender) =
        zebra_network::init(network_config, inbound_service, tip, user_agent).await;
    // Keep the peer set in scope so zebra-network keeps crawling for the
    // lifetime of the seeder.

    let rate_limiter = config
        .rate_limit
        .as_ref()
        .map(RateLimiter::new)
        .transpose()?;

    tracing::info!("Initializing DNS server on {}", config.dns.listen_addr);

    let servable_peers = address_cache::spawn(address_book.clone(), network);

    let request_handler = DnsRequestHandler::new(servable_peers, seed_zone, rate_limiter);
    let mut server = Server::new(request_handler);

    server.register_socket(dns_sockets.udp_socket);
    server.register_listener(dns_sockets.tcp_listener, Duration::from_secs(5), 32);

    tracing::info!("Seeder running. Press Ctrl+C or send SIGTERM to exit.");

    tokio::select! {
        biased;

        signal = shutdown_signal() => {
            let signal = signal?;
            tracing::info!(%signal, "received shutdown signal, cleaning up");
            tracing::info!("Cleanup complete");
        }
        dns_outcome = server.block_until_done() => {
            dns_outcome.wrap_err("DNS server crashed")?;
            tracing::info!("DNS server stopped, shutting down...");
        }
    }

    drop(peer_set);

    Ok(())
}

async fn bind_dns_sockets(listen_addr: SocketAddr) -> Result<DnsSockets> {
    let udp_socket = UdpSocket::bind(listen_addr)
        .await
        .wrap_err_with(|| dns_bind_error("UDP socket", listen_addr))?;
    let tcp_listener = TcpListener::bind(listen_addr)
        .await
        .wrap_err_with(|| dns_bind_error("TCP listener", listen_addr))?;

    tracing::info!("DNS sockets bound on {listen_addr}");

    Ok(DnsSockets {
        udp_socket,
        tcp_listener,
    })
}

fn dns_bind_error(socket_kind: &str, listen_addr: SocketAddr) -> String {
    let privileged_port_hint = if listen_addr.port() < 1024 {
        " This is a privileged port; run with permission to bind it or forward traffic to a high port."
    } else {
        ""
    };

    format!("failed to bind DNS {socket_kind} on {listen_addr}.{privileged_port_hint}")
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm =
        signal(SignalKind::terminate()).wrap_err("failed to install SIGTERM handler")?;

    tokio::select! {
        biased;

        _ = sigterm.recv() => Ok("SIGTERM"),
        signal_result = tokio::signal::ctrl_c() => {
            signal_result.wrap_err("failed to listen for Ctrl+C")?;
            Ok("SIGINT")
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<&'static str> {
    tokio::signal::ctrl_c()
        .await
        .wrap_err("failed to listen for Ctrl+C")?;
    Ok("SIGINT")
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::*;

    #[test]
    fn dns_bind_error_includes_configured_address() {
        let error = dns_bind_error(
            "UDP socket",
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19056),
        );

        assert!(error.contains("127.0.0.1:19056"));
        assert!(
            !error.contains("privileged port"),
            "high-port bind errors should not mention privileged ports"
        );
    }

    #[test]
    fn privileged_dns_bind_error_includes_hint() {
        let error = dns_bind_error(
            "UDP socket",
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 53),
        );

        assert!(error.contains("0.0.0.0:53"));
        assert!(
            error.contains("privileged port"),
            "port-53 bind errors should include an operator hint"
        );
    }
}
