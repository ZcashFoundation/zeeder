//! Peer eligibility: the single decision for whether a crawled peer should be
//! served to a bootstrapping node.
//!
//! A peer is *servable* only when zebra-network has recently completed a
//! handshake with it. A recent handshake transitively proves the peer passed
//! zebra-network's protocol-version floor (it would otherwise have been rejected
//! with `ObsoleteVersion`) and advertised the network service. On top of that we
//! require a routable address on the network's default port, no ban, and no
//! recorded misbehavior.
//!
//! The branch logic lives in [`classify`], which is pure over primitives so
//! every reason is unit-testable without constructing a live [`MetaAddr`].
//! [`classify_peer`] is the thin adapter that extracts those primitives from an
//! address-book entry.

use std::{collections::HashSet, net::IpAddr};

use chrono::{DateTime, Utc};
use zebra_network::types::MetaAddr;

/// Why a peer is not servable.
///
/// Each variant maps to a stable snake_case `reason` metric label so operators
/// (and agents) can see exactly why the served set is the size it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IneligibleReason {
    /// Loopback, unspecified, or multicast address.
    NotRoutable,
    /// Not on the network's default port. DNS answers cannot convey a port, so
    /// only default-port peers are reachable by clients that resolve us.
    WrongPort,
    /// The peer's IP is banned by zebra-network.
    Banned,
    /// zebra-network has recorded misbehavior against the peer.
    Misbehaving,
    /// No recent successful handshake: gossiped-but-never-contacted, failed, or
    /// stale peers. This is the gate that keeps unverified addresses out.
    NotRecentlyLive,
}

impl IneligibleReason {
    /// Every reason, so callers can zero each per-reason gauge on every refresh
    /// (a reason with no peers this cycle must report `0`, not a stale value).
    pub const ALL: [IneligibleReason; 5] = [
        Self::NotRoutable,
        Self::WrongPort,
        Self::Banned,
        Self::Misbehaving,
        Self::NotRecentlyLive,
    ];

    /// Stable snake_case label used as a metric value.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotRoutable => "not_routable",
            Self::WrongPort => "wrong_port",
            Self::Banned => "banned",
            Self::Misbehaving => "misbehaving",
            Self::NotRecentlyLive => "not_recently_live",
        }
    }
}

/// Decide servability from a peer's already-extracted attributes.
///
/// Pure over primitives so every branch is unit-testable. The check order also
/// fixes which reason is attributed when several fail at once: the cheapest,
/// most structural reasons first, liveness last (the dominant reason in
/// practice, since most address-book entries are unverified gossip).
fn classify(
    ip: IpAddr,
    port: u16,
    default_port: u16,
    is_banned: bool,
    misbehavior_score: u32,
    is_recently_live: bool,
) -> Result<(), IneligibleReason> {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return Err(IneligibleReason::NotRoutable);
    }
    if port != default_port {
        return Err(IneligibleReason::WrongPort);
    }
    if is_banned {
        return Err(IneligibleReason::Banned);
    }
    if misbehavior_score > 0 {
        return Err(IneligibleReason::Misbehaving);
    }
    if !is_recently_live {
        return Err(IneligibleReason::NotRecentlyLive);
    }
    Ok(())
}

/// Classify one address-book entry at instant `now`.
///
/// `banned` is the set of IPs zebra-network is currently dropping. Membership is
/// the ban signal: zebra records a timestamp per banned IP but checks bans by
/// presence, not expiry, so we mirror that.
pub fn classify_peer(
    meta: &MetaAddr,
    now: DateTime<Utc>,
    default_port: u16,
    banned: &HashSet<IpAddr>,
) -> Result<(), IneligibleReason> {
    let addr = meta.addr();
    classify(
        addr.ip(),
        addr.port(),
        default_port,
        banned.contains(&addr.ip()),
        meta.misbehavior(),
        meta.was_recently_live(now),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_PORT: u16 = 8233;

    fn routable_ip() -> IpAddr {
        "8.8.8.8".parse().unwrap()
    }

    #[test]
    fn servable_when_every_condition_is_met() {
        assert_eq!(
            classify(routable_ip(), DEFAULT_PORT, DEFAULT_PORT, false, 0, true),
            Ok(())
        );
    }

    #[test]
    fn non_routable_addresses_are_rejected() {
        for raw in ["127.0.0.1", "0.0.0.0", "224.0.0.1", "::1", "::", "ff02::1"] {
            let ip: IpAddr = raw.parse().unwrap();
            assert_eq!(
                classify(ip, DEFAULT_PORT, DEFAULT_PORT, false, 0, true),
                Err(IneligibleReason::NotRoutable),
                "{raw} should be NotRoutable"
            );
        }
    }

    #[test]
    fn non_default_port_is_rejected() {
        assert_eq!(
            classify(routable_ip(), 1234, DEFAULT_PORT, false, 0, true),
            Err(IneligibleReason::WrongPort)
        );
    }

    #[test]
    fn banned_ip_is_rejected() {
        assert_eq!(
            classify(routable_ip(), DEFAULT_PORT, DEFAULT_PORT, true, 0, true),
            Err(IneligibleReason::Banned)
        );
    }

    #[test]
    fn misbehaving_peer_is_rejected() {
        assert_eq!(
            classify(routable_ip(), DEFAULT_PORT, DEFAULT_PORT, false, 1, true),
            Err(IneligibleReason::Misbehaving)
        );
    }

    #[test]
    fn peer_without_recent_handshake_is_rejected() {
        assert_eq!(
            classify(routable_ip(), DEFAULT_PORT, DEFAULT_PORT, false, 0, false),
            Err(IneligibleReason::NotRecentlyLive)
        );
    }

    #[test]
    fn structural_reasons_take_precedence_over_liveness() {
        // A loopback peer that also fails every other check still reports the
        // most structural reason first, so the metric attribution is stable.
        assert_eq!(
            classify(
                "127.0.0.1".parse().unwrap(),
                1234,
                DEFAULT_PORT,
                true,
                9,
                false
            ),
            Err(IneligibleReason::NotRoutable)
        );
    }

    #[test]
    fn every_reason_has_a_unique_label() {
        let labels: Vec<&str> = IneligibleReason::ALL.iter().map(|r| r.label()).collect();
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len(), "reason labels must be unique");
        assert_eq!(unique.len(), 5, "all reasons must have a label");
    }
}
