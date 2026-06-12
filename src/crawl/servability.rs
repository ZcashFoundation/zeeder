//! Peer servability: whether a crawled peer should be served to a bootstrapping
//! node.
//!
//! A peer is servable only when zebra-network has recently handshaked it (which
//! proves it passed the protocol-version floor), it advertises the full-node
//! `NODE_NETWORK` service, and its address is routable on the network's default
//! port. The handshake enforces the version floor but not `NODE_NETWORK`, so the
//! service is checked here.

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
}

impl UnservableReason {
    /// Every reason, so callers can reset each per-reason gauge on a refresh.
    pub(crate) const ALL: [Self; 4] = [
        Self::NotRoutable,
        Self::WrongPort,
        Self::NotRecentlyLive,
        Self::NotFullNode,
    ];

    /// Stable `snake_case` label used as a metric value.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NotRoutable => "not_routable",
            Self::WrongPort => "wrong_port",
            Self::NotRecentlyLive => "not_recently_live",
            Self::NotFullNode => "not_full_node",
        }
    }
}

/// Decide servability from a peer's attributes.
///
/// Order matters: it fixes which reason is reported when several checks fail.
fn classify(
    ip: IpAddr,
    port: u16,
    default_port: u16,
    is_recently_live: bool,
    advertises_node_network: bool,
) -> Result<(), UnservableReason> {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return Err(UnservableReason::NotRoutable);
    }
    if port != default_port {
        return Err(UnservableReason::WrongPort);
    }
    if !is_recently_live {
        return Err(UnservableReason::NotRecentlyLive);
    }
    if !advertises_node_network {
        return Err(UnservableReason::NotFullNode);
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
    classify(
        addr.ip(),
        addr.port(),
        network.default_port(),
        meta.was_recently_live(now),
        meta.last_known_info_is_valid_for_outbound(network),
    )
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

    #[test]
    fn servable_when_every_condition_is_met() {
        assert_eq!(
            classify(routable_ip(), DEFAULT_PORT, DEFAULT_PORT, true, true),
            Ok(())
        );
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
            assert_eq!(
                classify(ip, DEFAULT_PORT, DEFAULT_PORT, true, true),
                Err(UnservableReason::NotRoutable),
                "{ip} should be NotRoutable"
            );
        }
    }

    #[test]
    fn non_default_port_is_rejected() {
        assert_eq!(
            classify(routable_ip(), 1234, DEFAULT_PORT, true, true),
            Err(UnservableReason::WrongPort)
        );
    }

    #[test]
    fn peer_without_recent_handshake_is_rejected() {
        assert_eq!(
            classify(routable_ip(), DEFAULT_PORT, DEFAULT_PORT, false, true),
            Err(UnservableReason::NotRecentlyLive)
        );
    }

    #[test]
    fn recently_live_non_full_node_is_rejected() {
        assert_eq!(
            classify(routable_ip(), DEFAULT_PORT, DEFAULT_PORT, true, false),
            Err(UnservableReason::NotFullNode)
        );
    }

    #[test]
    fn structural_reasons_take_precedence_over_liveness() {
        assert_eq!(
            classify(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                1234,
                DEFAULT_PORT,
                false,
                false
            ),
            Err(UnservableReason::NotRoutable)
        );
    }

    #[test]
    fn every_reason_has_a_unique_label() {
        let labels: Vec<&str> = UnservableReason::ALL.iter().map(|r| r.label()).collect();
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len(), "reason labels must be unique");
        assert_eq!(unique.len(), 4, "all reasons must have a label");
    }
}
