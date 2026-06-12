//! Process composition for zebra-network crawling and DNS serving.

use std::time::Duration;

use color_eyre::eyre::{Context, Result};
use hickory_server::server::Server;
use metrics::gauge;
use tokio::net::{TcpListener, UdpSocket};
use zebra_chain::chain_tip::ChainTip;

use crate::{
    config::SeederConfig,
    crawl::{address_cache, chain_tip},
    dns::{rate_limiter::RateLimiter, request_handler::DnsRequestHandler},
};

/// Run the seeder until the DNS server exits or the process receives Ctrl+C.
pub(crate) async fn run(config: SeederConfig) -> Result<()> {
    tracing::info!("Initializing zebra-network...");

    // Dummy inbound service that rejects everything.
    let inbound_service = tower::service_fn(|_req: zebra_network::Request| async move {
        Ok::<zebra_network::Response, Box<dyn std::error::Error + Send + Sync + 'static>>(
            zebra_network::Response::Nil,
        )
    });

    let user_agent = option_env!("VERGEN_GIT_SHA").map_or_else(
        || format!("zebra-seeder/{}", env!("CARGO_PKG_VERSION")),
        |sha| {
            let short_sha = &sha[..7.min(sha.len())];
            format!("zebra-seeder/{} ({short_sha})", env!("CARGO_PKG_VERSION"))
        },
    );

    tracing::info!("User-Agent: {user_agent}");

    // Pin a chain tip at the current network upgrade so zebra-network's
    // handshake rejects peers advertising an outdated protocol version.
    let tip = chain_tip::SeederChainTip::at_current_upgrade(&config.network.network);
    let min_protocol_version = zebra_network::Version::min_remote_for_height(
        &config.network.network,
        tip.best_tip_height(),
    );
    tracing::info!(
        network = %config.network.network,
        %min_protocol_version,
        "enforcing peer protocol-version floor"
    );
    gauge!("seeder_min_protocol_version").set(f64::from(min_protocol_version.0));
    gauge!(
        "seeder_build_info",
        "version" => env!("CARGO_PKG_VERSION"),
        "network" => config.network.network.to_string(),
    )
    .set(1.0);

    let (peer_set, address_book, _misbehavior_sender) =
        zebra_network::init(config.network.clone(), inbound_service, tip, user_agent).await;
    // Keep the peer set in scope so zebra-network keeps crawling for the
    // lifetime of the seeder.

    let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);

    tracing::info!("Initializing DNS server on {}", config.dns_listen_addr);

    let servable_peers = address_cache::spawn(address_book.clone(), config.network.network.clone());

    let request_handler = DnsRequestHandler::new(
        servable_peers,
        config.seed_domain.clone(),
        config.dns_ttl,
        rate_limiter,
    );
    let mut server = Server::new(request_handler);

    let udp_socket = UdpSocket::bind(config.dns_listen_addr)
        .await
        .wrap_err("failed to bind UDP socket")?;
    server.register_socket(udp_socket);

    let tcp_listener = TcpListener::bind(config.dns_listen_addr)
        .await
        .wrap_err("failed to bind TCP listener")?;
    server.register_listener(tcp_listener, Duration::from_secs(5), 32);

    tracing::info!("Seeder running. Press Ctrl+C to exit.");

    tokio::select! {
        dns_outcome = server.block_until_done() => {
            dns_outcome.wrap_err("DNS server crashed")?;
            tracing::info!("DNS server stopped, shutting down...");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal, cleaning up...");
            tracing::info!("Cleanup complete");
        }
    }

    drop(peer_set);

    Ok(())
}
