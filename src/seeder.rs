//! Process composition: one zebra-network crawler per configured network, all
//! served as DNS zones on a single shared listener.

use std::{net::SocketAddr, time::Duration};

use color_eyre::eyre::{Context, Result};
use hickory_server::server::Server;
use metrics::gauge;
use tokio::net::{TcpListener, UdpSocket};
use zebra_chain::chain_tip::ChainTip;

use crate::{
    build_info,
    config::{SeederConfig, ZcashNetwork, ZoneConfig},
    crawl::{activation, address_cache, chain_tip},
    dns::{
        rate_limiter::RateLimiter,
        request_handler::{DnsRequestHandler, SeedZone},
    },
    health,
    metrics::{BUILD_INFO, LABEL_GIT_SHA, LABEL_NETWORK, LABEL_VERSION, MIN_PROTOCOL_VERSION},
};

struct DnsSockets {
    udp_socket: UdpSocket,
    tcp_listener: TcpListener,
}

/// Run the seeder until the DNS server exits or the process receives a shutdown signal.
pub(crate) async fn run(config: SeederConfig) -> Result<()> {
    let dns_sockets = bind_dns_sockets(config.dns.listen_addr).await?;

    let user_agent = build_info::user_agent();
    tracing::info!("User-Agent: {user_agent}");

    let mut seed_zones = Vec::with_capacity(config.zones.len());
    let mut crawler_guards = Vec::with_capacity(config.zones.len());

    for (network, zone) in &config.zones {
        let (seed_zone, crawler_guard) =
            spawn_network_crawler(*network, zone, user_agent.clone()).await?;
        seed_zones.push(seed_zone);
        crawler_guards.push(crawler_guard);
    }

    let rate_limiter = config
        .rate_limit
        .as_ref()
        .map(RateLimiter::new)
        .transpose()?;

    if let Some(health_config) = &config.health {
        health::spawn(
            health_config.endpoint_addr,
            health_config.ready_threshold,
            seed_zones.iter().map(SeedZone::readiness).collect(),
        )
        .await?;
    }

    tracing::info!("Initializing DNS server on {}", config.dns.listen_addr);

    let request_handler = DnsRequestHandler::new(seed_zones, rate_limiter);
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

    // Dropping each crawler guard aborts that network's crawl tasks.
    drop(crawler_guards);

    Ok(())
}

/// Start one network's crawler and wire its servable-peer cache to a seed zone.
///
/// Returns the seed zone the DNS handler serves and an opaque guard the caller
/// must keep alive; dropping the guard stops this network's crawl.
async fn spawn_network_crawler(
    network: ZcashNetwork,
    zone: &ZoneConfig,
    user_agent: String,
) -> Result<(SeedZone, impl Send + 'static)> {
    let network_config = network.network_config();
    let zcash_network = network_config.network.clone();
    let network_label = network.label();
    let activation_target = activation::ActivationTarget::latest(&zcash_network);
    let confirmation_path =
        activation::confirmation_path(&network_config.cache_dir, &zcash_network);
    let activation_confirmed =
        activation::load_confirmation(confirmation_path.as_deref(), activation_target).await;

    tracing::info!(
        network = network_label,
        "Initializing zebra-network crawler"
    );

    // Dummy inbound service that rejects everything.
    let inbound_service = tower::service_fn(|_req: zebra_network::Request| async move {
        Ok::<zebra_network::Response, Box<dyn std::error::Error + Send + Sync + 'static>>(
            zebra_network::Response::Nil,
        )
    });

    // Keep the previous upgrade's floor until independent observations confirm
    // that the compiled activation is safely buried, or load that exact
    // confirmation from the local cache after a restart.
    let tip = chain_tip::SeederChainTip::new(activation_target, activation_confirmed);
    let min_protocol_version =
        zebra_network::Version::min_remote_for_height(&zcash_network, tip.best_tip_height());
    tracing::info!(
        network = network_label,
        %min_protocol_version,
        activation_confirmed,
        "enforcing observed peer protocol-version floor"
    );
    gauge!(MIN_PROTOCOL_VERSION, LABEL_NETWORK => network_label)
        .set(f64::from(min_protocol_version.0));
    gauge!(
        BUILD_INFO,
        LABEL_VERSION => build_info::VERSION,
        LABEL_GIT_SHA => build_info::git_sha_label(),
        LABEL_NETWORK => network_label,
    )
    .set(1.0);

    let (peer_set, address_book, misbehavior_sender) = zebra_network::init(
        network_config,
        inbound_service,
        tip.clone(),
        user_agent.clone(),
    )
    .await;

    let servable_peers = address_cache::spawn(address_book.clone(), network, tip.clone());
    let activation_observer = activation::spawn(
        address_book,
        network,
        user_agent,
        tip,
        activation_target,
        confirmation_path,
    );

    let seed_zone = SeedZone::new(
        network,
        &zone.domain,
        &zone.nameserver,
        zone.ttl,
        servable_peers,
    )?;

    // Dropping this guard aborts both the crawler and activation observer;
    // `misbehavior_sender` also keeps zebra-network's batch task alive.
    let crawler_guard = (peer_set, misbehavior_sender, activation_observer);

    Ok((seed_zone, crawler_guard))
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
