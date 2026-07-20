//! A fixed chain tip that pins zebra-network's peer protocol-version floor.
//!
//! zebra-network derives the minimum protocol version it accepts during a
//! handshake from the chain tip's height (`Version::min_remote_for_height`).
//! [`SeederChainTip`] reports the activation height of the network's current
//! upgrade, so the handshake rejects peers below that upgrade's floor (NU6.3,
//! `170160`, on Mainnet and Testnet) and they never reach the address book.
//!
//! Only `best_tip_height` feeds that floor; the networking path reads no other
//! [`ChainTip`] accessor, so the rest return `None` and `best_tip_changed`
//! pends forever.

use std::{future, sync::Arc};

use chrono::{DateTime, Utc};
use zebra_chain::{
    BoxError,
    block::{self, Height},
    chain_tip::ChainTip,
    parameters::{Network, NetworkUpgrade},
    transaction,
};

/// A chain tip frozen at the activation height of the network's current upgrade.
#[derive(Clone, Debug)]
pub(crate) struct SeederChainTip {
    height: Height,
}

impl SeederChainTip {
    /// Pin the tip to the activation height of `network`'s current
    /// (highest-activated) network upgrade.
    ///
    /// The height comes from zebra-chain's activation table rather than a
    /// hardcoded constant, so the enforced version floor rises automatically
    /// when a future zebra-chain release activates the next upgrade.
    pub(crate) fn at_current_upgrade(network: &Network) -> Self {
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

    /// The tip is fixed, so a change is never signalled.
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
        let tip = SeederChainTip::at_current_upgrade(network);
        Version::min_remote_for_height(network, tip.best_tip_height())
    }

    fn no_chain_tip_floor(network: &Network) -> Version {
        Version::min_remote_for_height(network, None)
    }

    #[test]
    fn mainnet_floor_is_the_current_network_upgrade() {
        // Pins the floor so a zebra bump that activates the next upgrade trips here.
        assert_eq!(version_floor(&Network::Mainnet), Version(170_160));
    }

    #[test]
    fn testnet_floor_is_the_current_network_upgrade() {
        // Pins the floor so a zebra bump that activates the next upgrade trips here.
        assert_eq!(
            version_floor(&Network::new_default_testnet()),
            Version(170_160)
        );
    }

    #[test]
    fn chain_tip_does_not_lower_no_chain_tip_fallback() {
        // Zebra's no-tip fallback can already match the current upgrade floor.
        // The fixed tip must never lower it, and still lets future activation
        // table updates raise the floor automatically.
        for network in [Network::Mainnet, Network::new_default_testnet()] {
            assert!(
                version_floor(&network) >= no_chain_tip_floor(&network),
                "{network} floor must not be below the NoChainTip fallback"
            );
        }
    }
}
