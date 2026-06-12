//! Peer servability: whether a crawled peer should be served to a bootstrapping
//! node.
//!
//! A peer is servable only when zebra-network has recently handshaked it (which
//! proves it passed the protocol-version floor), it advertises the full-node
//! `NODE_NETWORK` service, its address is routable on the network's default
//! port, it was not recorded from an inbound connection, and it has no
//! misbehavior score. The handshake enforces the version floor but not
//! `NODE_NETWORK`, so the service is checked here.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use zebra_chain::parameters::Network;
use zebra_network::types::MetaAddr;

/// Why a peer is not servable. Each variant maps to a stable `reason` metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum UnservableReason {
    /// Loopback, unspecified, or multicast address.
    NotRoutable,
    /// Not on the network's default port (DNS answers cannot carry a port).
    WrongPort,
    /// No recent successful handshake (gossiped, failed, or stale).
    NotRecentlyLive,
    /// Does not advertise the full-node (`NODE_NETWORK`) service.
    NotFullNode,
    /// Recorded from an inbound peer connection.
    Inbound,
    /// Has a non-zero zebra-network misbehavior score.
    Misbehaving,
}

impl UnservableReason {
    /// Every reason, so callers can reset each per-reason gauge on a refresh.
    pub(crate) const ALL: [Self; 6] = [
        Self::NotRoutable,
        Self::WrongPort,
        Self::NotRecentlyLive,
        Self::NotFullNode,
        Self::Inbound,
        Self::Misbehaving,
    ];

    /// Stable `snake_case` label used as a metric value.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NotRoutable => "not_routable",
            Self::WrongPort => "wrong_port",
            Self::NotRecentlyLive => "not_recently_live",
            Self::NotFullNode => "not_full_node",
            Self::Inbound => "inbound",
            Self::Misbehaving => "misbehaving",
        }
    }
}

#[derive(Clone, Copy)]
struct PeerAttributes {
    ip: IpAddr,
    port: u16,
    default_port: u16,
    is_recently_live: bool,
    advertises_node_network: bool,
    is_inbound: bool,
    misbehavior_score: u32,
}

/// Decide servability from a peer's attributes.
///
/// Order matters: it fixes which reason is reported when several checks fail.
fn classify(peer: PeerAttributes) -> Result<(), UnservableReason> {
    if peer.ip.is_loopback() || peer.ip.is_unspecified() || peer.ip.is_multicast() {
        return Err(UnservableReason::NotRoutable);
    }
    if peer.port != peer.default_port {
        return Err(UnservableReason::WrongPort);
    }
    if !peer.is_recently_live {
        return Err(UnservableReason::NotRecentlyLive);
    }
    if !peer.advertises_node_network {
        return Err(UnservableReason::NotFullNode);
    }
    if peer.is_inbound {
        return Err(UnservableReason::Inbound);
    }
    if peer.misbehavior_score != 0 {
        return Err(UnservableReason::Misbehaving);
    }
    Ok(())
}

/// Extract the values [`classify`] needs from an address-book entry.
pub(crate) fn classify_peer(
    meta: &MetaAddr,
    now: DateTime<Utc>,
    network: &Network,
) -> Result<(), UnservableReason> {
    let addr = meta.addr();
    classify(PeerAttributes {
        ip: addr.ip(),
        port: addr.port(),
        default_port: network.default_port(),
        is_recently_live: meta.was_recently_live(now),
        advertises_node_network: meta.last_known_info_is_valid_for_outbound(network),
        is_inbound: meta.is_inbound(),
        misbehavior_score: meta.misbehavior(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    const DEFAULT_PORT: u16 = 8233;

    fn routable_ip() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))
    }

    fn servable_peer() -> PeerAttributes {
        PeerAttributes {
            ip: routable_ip(),
            port: DEFAULT_PORT,
            default_port: DEFAULT_PORT,
            is_recently_live: true,
            advertises_node_network: true,
            is_inbound: false,
            misbehavior_score: 0,
        }
    }

    #[test]
    fn servable_when_every_condition_is_met() {
        assert_eq!(classify(servable_peer()), Ok(()));
    }

    #[test]
    fn non_routable_addresses_are_rejected() {
        let cases = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1)),
        ];
        for ip in cases {
            let mut peer = servable_peer();
            peer.ip = ip;

            assert_eq!(
                classify(peer),
                Err(UnservableReason::NotRoutable),
                "{ip} should be NotRoutable"
            );
        }
    }

    #[test]
    fn non_default_port_is_rejected() {
        let mut peer = servable_peer();
        peer.port = 1234;

        assert_eq!(classify(peer), Err(UnservableReason::WrongPort));
    }

    #[test]
    fn peer_without_recent_handshake_is_rejected() {
        let mut peer = servable_peer();
        peer.is_recently_live = false;

        assert_eq!(classify(peer), Err(UnservableReason::NotRecentlyLive));
    }

    #[test]
    fn recently_live_non_full_node_is_rejected() {
        let mut peer = servable_peer();
        peer.advertises_node_network = false;

        assert_eq!(classify(peer), Err(UnservableReason::NotFullNode));
    }

    #[test]
    fn inbound_peer_is_rejected() {
        let mut peer = servable_peer();
        peer.is_inbound = true;

        assert_eq!(classify(peer), Err(UnservableReason::Inbound));
    }

    #[test]
    fn misbehaving_peer_is_rejected() {
        let mut peer = servable_peer();
        peer.misbehavior_score = 1;

        assert_eq!(classify(peer), Err(UnservableReason::Misbehaving));
    }

    #[test]
    fn structural_reasons_take_precedence_over_liveness() {
        let mut peer = servable_peer();
        peer.ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        peer.port = 1234;
        peer.is_recently_live = false;
        peer.advertises_node_network = false;
        peer.is_inbound = true;
        peer.misbehavior_score = 1;

        assert_eq!(classify(peer), Err(UnservableReason::NotRoutable));
    }

    #[test]
    fn every_reason_has_a_unique_label() {
        let labels: Vec<&str> = UnservableReason::ALL.iter().map(|r| r.label()).collect();
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len(), "reason labels must be unique");
        assert_eq!(unique.len(), 6, "all reasons must have a label");
    }
}
