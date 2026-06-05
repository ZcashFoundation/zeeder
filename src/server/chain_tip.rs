//! A fixed chain tip that pins zebra-network's peer protocol-version floor.
//!
//! The seeder keeps no block state, but zebra-network derives the minimum
//! protocol version it accepts from a peer during the handshake
//! (`Version::min_remote_for_height`) from the chain tip it is given. With
//! [`NoChainTip`](zebra_chain::chain_tip::NoChainTip) that floor collapses to
//! the static pre-NU6 minimum, so the handshake accepts peers several upgrades
//! behind, and those peers then get served.
//!
//! [`SeederChainTip`] reports the activation height of the network's current
//! (highest-activated) upgrade instead, so zebra-network rejects any peer
//! advertising a protocol version below that upgrade's minimum. Such peers never
//! reach the address book, so the eligibility filter never sees them.

use std::{future, sync::Arc};

use chrono::{DateTime, Utc};
use zebra_chain::{
    block::{self, Height},
    chain_tip::ChainTip,
    parameters::{Network, NetworkUpgrade},
    transaction, BoxError,
};

/// A chain tip frozen at the activation height of the network's current upgrade.
#[derive(Clone, Debug)]
pub struct SeederChainTip {
    height: Height,
}

impl SeederChainTip {
    /// Pin the tip to the activation height of `network`'s current
    /// (highest-activated) network upgrade.
    ///
    /// The height comes from zebra-chain's activation table rather than a
    /// hardcoded constant, so the enforced version floor rises automatically
    /// when a future zebra-chain release activates the next upgrade.
    pub fn current_upgrade(network: &Network) -> Self {
        let (_upgrade, height) =
            NetworkUpgrade::current_with_activation_height(network, Height::MAX);
        Self { height }
    }
}

impl ChainTip for SeederChainTip {
    fn best_tip_height(&self) -> Option<Height> {
        Some(self.height)
    }

    fn best_tip_hash(&self) -> Option<block::Hash> {
        None
    }

    fn best_tip_height_and_hash(&self) -> Option<(Height, block::Hash)> {
        None
    }

    fn best_tip_block_time(&self) -> Option<DateTime<Utc>> {
        None
    }

    fn best_tip_height_and_block_time(&self) -> Option<(Height, DateTime<Utc>)> {
        None
    }

    fn best_tip_mined_transaction_ids(&self) -> Arc<[transaction::Hash]> {
        Arc::new([])
    }

    /// The tip is fixed, so a change is never signalled. Resolving here would
    /// make zebra-network busy-loop recomputing the unchanged minimum version.
    async fn best_tip_changed(&mut self) -> Result<(), BoxError> {
        future::pending().await
    }

    fn mark_best_tip_seen(&mut self) {}
}

#[cfg(test)]
mod tests {
    use zebra_network::Version;

    use super::*;

    fn version_floor(network: &Network) -> Version {
        let tip = SeederChainTip::current_upgrade(network);
        Version::min_remote_for_height(network, tip.best_tip_height())
    }

    fn no_chain_tip_floor(network: &Network) -> Version {
        Version::min_remote_for_height(network, None)
    }

    #[test]
    fn mainnet_floor_is_the_current_network_upgrade() {
        // Pins the expected Mainnet floor so a zebra bump that activates the
        // next upgrade fails here and forces a conscious review.
        // Currently NU6.2 (protocol version 170_150).
        assert_eq!(version_floor(&Network::Mainnet), Version(170_150));
    }

    #[test]
    fn chain_tip_raises_floor_above_no_chain_tip_fallback() {
        // The whole point of the fixed tip: it must lift the handshake floor
        // above the static pre-NU6 minimum that NoChainTip would leave in place.
        for network in [Network::Mainnet, Network::new_default_testnet()] {
            assert!(
                version_floor(&network) > no_chain_tip_floor(&network),
                "{network} floor must exceed the NoChainTip fallback"
            );
        }
    }
}
