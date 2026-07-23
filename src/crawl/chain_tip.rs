//! A watch-backed chain tip that advances only after independent activation
//! observation.
//!
//! zebra-network derives the minimum protocol version it accepts during a
//! handshake from the chain tip's height (`Version::min_remote_for_height`).
//! [`SeederChainTip`] starts immediately below the newest compiled activation,
//! keeping the previous upgrade's floor. The activation observer advances it
//! only after a fixed, sustained quorum reports the activation safely buried.
//!
//! Only `best_tip_height` and `best_tip_changed` feed that floor; the other
//! [`ChainTip`] accessors return no chain data because Zeeder remains stateless.

use std::{io, sync::Arc};

use chrono::{DateTime, Utc};
use tokio::sync::watch;
use zebra_chain::{
    BoxError,
    block::{self, Height},
    chain_tip::ChainTip,
    transaction,
};

use crate::crawl::activation::ActivationTarget;

/// The activation height currently trusted by zebra-network.
#[derive(Clone, Debug)]
pub(crate) struct SeederChainTip {
    activation_height: Height,
    sender: Arc<watch::Sender<Height>>,
    receiver: watch::Receiver<Height>,
}

impl SeederChainTip {
    /// Start at the previous upgrade, or at the target when already persisted.
    pub(crate) fn new(target: ActivationTarget, confirmed: bool) -> Self {
        let initial_height = if confirmed {
            target.activation_height
        } else {
            target.pre_activation_height
        };
        let (sender, receiver) = watch::channel(initial_height);

        Self {
            activation_height: target.activation_height,
            sender: Arc::new(sender),
            receiver,
        }
    }

    /// Advance to the observed activation height. This operation is monotone.
    pub(crate) fn confirm_activation(&self) {
        if *self.sender.borrow() < self.activation_height {
            self.sender.send_replace(self.activation_height);
        }
    }

    /// Return whether the observed or persisted activation has raised the tip.
    pub(crate) fn is_activation_confirmed(&self) -> bool {
        *self.receiver.borrow() >= self.activation_height
    }
}

impl ChainTip for SeederChainTip {
    fn best_tip_height(&self) -> Option<Height> {
        Some(*self.receiver.borrow())
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

    async fn best_tip_changed(&mut self) -> Result<(), BoxError> {
        self.receiver.changed().await.map_err(|error| {
            Box::new(io::Error::other(format!(
                "activation-height watch unexpectedly closed: {error}"
            ))) as BoxError
        })
    }

    fn mark_best_tip_seen(&mut self) {}
}

#[cfg(test)]
mod tests {
    use zebra_chain::parameters::Network;
    use zebra_network::Version;

    use super::*;
    use crate::crawl::activation::ActivationTarget;

    #[test]
    fn protocol_floor_changes_only_after_activation_confirmation() {
        let network = Network::Mainnet;
        let target = ActivationTarget::latest(&network);
        let tip = SeederChainTip::new(target, false);

        assert_eq!(
            Version::min_remote_for_height(&network, tip.best_tip_height()),
            Version(170_150)
        );

        tip.confirm_activation();

        assert_eq!(
            Version::min_remote_for_height(&network, tip.best_tip_height()),
            Version(170_160)
        );
    }

    #[tokio::test]
    async fn activation_confirmation_notifies_zebra_network() -> Result<(), BoxError> {
        let target = ActivationTarget::latest(&Network::Mainnet);
        let tip = SeederChainTip::new(target, false);
        let mut subscriber = tip.clone();

        tip.confirm_activation();
        subscriber.best_tip_changed().await?;

        assert_eq!(subscriber.best_tip_height(), Some(target.activation_height));
        Ok(())
    }

    fn version_floor(network: &Network, confirmed: bool) -> Version {
        let tip = SeederChainTip::new(ActivationTarget::latest(network), confirmed);
        Version::min_remote_for_height(network, tip.best_tip_height())
    }

    fn no_chain_tip_floor(network: &Network) -> Version {
        Version::min_remote_for_height(network, None)
    }

    #[test]
    fn confirmed_mainnet_floor_matches_the_compiled_target() {
        assert_eq!(version_floor(&Network::Mainnet, true), Version(170_160));
    }

    #[test]
    fn confirmed_testnet_floor_matches_the_compiled_target() {
        assert_eq!(
            version_floor(&Network::new_default_testnet(), true),
            Version(170_160)
        );
    }

    #[test]
    fn chain_tip_does_not_lower_no_chain_tip_fallback() {
        // The pre-activation tip must not lower Zebra's previous-upgrade
        // fallback while the observer gathers evidence.
        for network in [Network::Mainnet, Network::new_default_testnet()] {
            assert!(
                version_floor(&network, false) >= no_chain_tip_floor(&network),
                "{network} floor must not be below the NoChainTip fallback"
            );
        }
    }
}
