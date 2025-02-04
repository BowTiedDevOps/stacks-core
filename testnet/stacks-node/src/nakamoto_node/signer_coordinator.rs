// Copyright (C) 2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use libsigner::v0::messages::{MinerSlotID, SignerMessage as SignerMessageV0};
use libsigner::{BlockProposal, SignerSession, StackerDBSession};
use stacks::burnchains::Burnchain;
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::burn::{BlockSnapshot, ConsensusHash};
use stacks::chainstate::nakamoto::{NakamotoBlock, NakamotoChainState};
use stacks::chainstate::stacks::boot::{RewardSet, MINERS_NAME};
use stacks::chainstate::stacks::db::StacksChainState;
use stacks::chainstate::stacks::Error as ChainstateError;
use stacks::codec::StacksMessageCodec;
use stacks::libstackerdb::StackerDBChunkData;
use stacks::net::stackerdb::StackerDBs;
use stacks::types::chainstate::{StacksBlockId, StacksPrivateKey};
use stacks::util::hash::Sha512Trunc256Sum;
use stacks::util::secp256k1::MessageSignature;
use stacks::util_lib::boot::boot_code_id;

use super::stackerdb_listener::StackerDBListenerComms;
use super::Error as NakamotoNodeError;
use crate::event_dispatcher::StackerDBChannel;
use crate::nakamoto_node::stackerdb_listener::{StackerDBListener, EVENT_RECEIVER_POLL};
use crate::neon::Counters;
use crate::Config;

/// The state of the signer database listener, used by the miner thread to
/// interact with the signer listener.
pub struct SignerCoordinator {
    /// The private key used to sign messages from the miner
    message_key: StacksPrivateKey,
    /// Is this mainnet?
    is_mainnet: bool,
    /// The session for writing to the miners contract in the stackerdb
    miners_session: StackerDBSession,
    /// The total weight of all signers
    total_weight: u32,
    /// The weight threshold for block approval
    weight_threshold: u32,
    /// Interface to the StackerDB listener thread's data
    stackerdb_comms: StackerDBListenerComms,
    /// Keep running flag for the signer DB listener thread
    keep_running: Arc<AtomicBool>,
    /// Handle for the signer DB listener thread
    listener_thread: Option<JoinHandle<()>>,
}

impl SignerCoordinator {
    /// Create a new `SignerCoordinator` instance.
    /// This will spawn a new thread to listen for messages from the signer DB.
    pub fn new(
        stackerdb_channel: Arc<Mutex<StackerDBChannel>>,
        node_keep_running: Arc<AtomicBool>,
        reward_set: &RewardSet,
        burn_tip: &BlockSnapshot,
        burnchain: &Burnchain,
        message_key: StacksPrivateKey,
        config: &Config,
    ) -> Result<Self, ChainstateError> {
        info!("SignerCoordinator: starting up");
        let keep_running = Arc::new(AtomicBool::new(true));

        // Create the stacker DB listener
        let mut listener = StackerDBListener::new(
            stackerdb_channel,
            node_keep_running.clone(),
            keep_running.clone(),
            reward_set,
            burn_tip,
            burnchain,
        )?;
        let is_mainnet = config.is_mainnet();
        let rpc_socket = config
            .node
            .get_rpc_loopback()
            .ok_or_else(|| ChainstateError::MinerAborted)?;
        let miners_contract_id = boot_code_id(MINERS_NAME, is_mainnet);
        let miners_session = StackerDBSession::new(&rpc_socket.to_string(), miners_contract_id);

        let mut sc = Self {
            message_key,
            is_mainnet,
            miners_session,
            total_weight: listener.total_weight,
            weight_threshold: listener.weight_threshold,
            stackerdb_comms: listener.get_comms(),
            keep_running,
            listener_thread: None,
        };

        // Spawn the signer DB listener thread
        let listener_thread = std::thread::Builder::new()
            .name("stackerdb_listener".to_string())
            .spawn(move || {
                if let Err(e) = listener.run() {
                    error!("StackerDBListener: exited with error: {e:?}");
                }
            })
            .map_err(|e| {
                error!("Failed to spawn stackerdb_listener thread: {e:?}");
                ChainstateError::MinerAborted
            })?;

        sc.listener_thread = Some(listener_thread);

        Ok(sc)
    }

    /// Send a message over the miners contract using a `StacksPrivateKey`
    #[allow(clippy::too_many_arguments)]
    pub fn send_miners_message<M: StacksMessageCodec>(
        miner_sk: &StacksPrivateKey,
        sortdb: &SortitionDB,
        tip: &BlockSnapshot,
        stackerdbs: &StackerDBs,
        message: M,
        miner_slot_id: MinerSlotID,
        is_mainnet: bool,
        miners_session: &mut StackerDBSession,
        election_sortition: &ConsensusHash,
    ) -> Result<(), String> {
        let Some(slot_range) = NakamotoChainState::get_miner_slot(sortdb, tip, election_sortition)
            .map_err(|e| format!("Failed to read miner slot information: {e:?}"))?
        else {
            return Err("No slot for miner".into());
        };

        let slot_id = slot_range
            .start
            .saturating_add(miner_slot_id.to_u8().into());
        if !slot_range.contains(&slot_id) {
            return Err("Not enough slots for miner messages".into());
        }
        // Get the LAST slot version number written to the DB. If not found, use 0.
        // Add 1 to get the NEXT version number
        // Note: we already check above for the slot's existence
        let miners_contract_id = boot_code_id(MINERS_NAME, is_mainnet);
        let slot_version = stackerdbs
            .get_slot_version(&miners_contract_id, slot_id)
            .map_err(|e| format!("Failed to read slot version: {e:?}"))?
            .unwrap_or(0)
            .saturating_add(1);
        let mut chunk = StackerDBChunkData::new(slot_id, slot_version, message.serialize_to_vec());
        chunk
            .sign(miner_sk)
            .map_err(|_| "Failed to sign StackerDB chunk")?;

        match miners_session.put_chunk(&chunk) {
            Ok(ack) => {
                if ack.accepted {
                    debug!("Wrote message to stackerdb: {ack:?}");
                    Ok(())
                } else {
                    Err(format!("{ack:?}"))
                }
            }
            Err(e) => Err(format!("{e:?}")),
        }
    }

    /// Propose a Nakamoto block and gather signatures for it.
    /// This function begins by sending a `BlockProposal` message to the
    /// signers, and then it waits for the signers to respond with their
    /// signatures. It does so in two ways, concurrently:
    /// * It waits for the signer DB listener to collect enough signatures to
    ///   accept or reject the block
    /// * It waits for the chainstate to contain the relayed block. If so, then its signatures are
    ///   loaded and returned. This can happen if the node receives the block via a signer who
    ///   fetched all signatures and assembled the signature vector, all before we could.
    // Mutants skip here: this function is covered via integration tests,
    //  which the mutation testing does not see.
    #[cfg_attr(test, mutants::skip)]
    #[allow(clippy::too_many_arguments)]
    pub fn propose_block(
        &mut self,
        block: &NakamotoBlock,
        burn_tip: &BlockSnapshot,
        burnchain: &Burnchain,
        sortdb: &SortitionDB,
        chain_state: &mut StacksChainState,
        stackerdbs: &StackerDBs,
        counters: &Counters,
        election_sortition: &ConsensusHash,
    ) -> Result<Vec<MessageSignature>, NakamotoNodeError> {
        // Add this block to the block status map.
        self.stackerdb_comms.insert_block(&block.header);

        let reward_cycle_id = burnchain
            .block_height_to_reward_cycle(burn_tip.block_height)
            .expect("FATAL: tried to initialize coordinator before first burn block height");

        let block_proposal = BlockProposal {
            block: block.clone(),
            burn_height: burn_tip.block_height,
            reward_cycle: reward_cycle_id,
        };

        let block_proposal_message = SignerMessageV0::BlockProposal(block_proposal);
        debug!("Sending block proposal message to signers";
            "signer_signature_hash" => %block.header.signer_signature_hash(),
        );
        Self::send_miners_message::<SignerMessageV0>(
            &self.message_key,
            sortdb,
            burn_tip,
            stackerdbs,
            block_proposal_message,
            MinerSlotID::BlockProposal,
            self.is_mainnet,
            &mut self.miners_session,
            election_sortition,
        )
        .map_err(NakamotoNodeError::SigningCoordinatorFailure)?;
        counters.bump_naka_proposed_blocks();

        #[cfg(test)]
        {
            info!(
                "SignerCoordinator: sent block proposal to .miners, waiting for test signing channel"
            );
            // In test mode, short-circuit waiting for the signers if the TEST_SIGNING
            //  channel has been created. This allows integration tests for the stacks-node
            //  independent of the stacks-signer.
            if let Some(signatures) =
                crate::tests::nakamoto_integrations::TestSigningChannel::get_signature()
            {
                debug!("Short-circuiting waiting for signers, using test signature");
                return Ok(signatures);
            }
        }

        self.get_block_status(
            &block.header.signer_signature_hash(),
            &block.block_id(),
            chain_state,
            sortdb,
            burn_tip,
            counters,
        )
    }

    /// Get the block status for a given block hash.
    /// If we have not yet received enough signatures for this block, this
    /// method will block until we do. If this block shows up in the staging DB
    /// before we have enough signatures, we will return the signatures from
    /// there. If a new burnchain tip is detected, we will return an error.
    fn get_block_status(
        &self,
        block_signer_sighash: &Sha512Trunc256Sum,
        block_id: &StacksBlockId,
        chain_state: &mut StacksChainState,
        sortdb: &SortitionDB,
        burn_tip: &BlockSnapshot,
        counters: &Counters,
    ) -> Result<Vec<MessageSignature>, NakamotoNodeError> {
        loop {
            let block_status = match self.stackerdb_comms.wait_for_block_status(
                block_signer_sighash,
                EVENT_RECEIVER_POLL,
                |status| {
                    status.total_weight_signed < self.weight_threshold
                        && status
                            .total_reject_weight
                            .saturating_add(self.weight_threshold)
                            <= self.total_weight
                },
            )? {
                Some(status) => status,
                None => {
                    // If we just received a timeout, we should check if the burnchain
                    // tip has changed or if we received this signed block already in
                    // the staging db.
                    debug!("SignerCoordinator: Timeout waiting for block signatures");

                    // Look in the nakamoto staging db -- a block can only get stored there
                    // if it has enough signing weight to clear the threshold.
                    if let Ok(Some((stored_block, _sz))) = chain_state
                        .nakamoto_blocks_db()
                        .get_nakamoto_block(block_id)
                        .map_err(|e| {
                            warn!(
                                "Failed to query chainstate for block: {e:?}";
                                "block_id" => %block_id,
                                "block_signer_sighash" => %block_signer_sighash,
                            );
                            e
                        })
                    {
                        debug!("SignCoordinator: Found signatures in relayed block");
                        counters.bump_naka_signer_pushed_blocks();
                        return Ok(stored_block.header.signer_signature);
                    }

                    if Self::check_burn_tip_changed(sortdb, burn_tip) {
                        debug!("SignCoordinator: Exiting due to new burnchain tip");
                        return Err(NakamotoNodeError::BurnchainTipChanged);
                    }

                    continue;
                }
            };

            if block_status
                .total_reject_weight
                .saturating_add(self.weight_threshold)
                > self.total_weight
            {
                info!(
                    "{}/{} signers vote to reject block",
                    block_status.total_reject_weight, self.total_weight;
                    "block_signer_sighash" => %block_signer_sighash,
                );
                counters.bump_naka_rejected_blocks();
                return Err(NakamotoNodeError::SignersRejected);
            } else if block_status.total_weight_signed >= self.weight_threshold {
                info!("Received enough signatures, block accepted";
                    "block_signer_sighash" => %block_signer_sighash,
                );
                return Ok(block_status.gathered_signatures.values().cloned().collect());
            } else {
                return Err(NakamotoNodeError::SigningCoordinatorFailure(
                    "Unblocked without reaching the threshold".into(),
                ));
            }
        }
    }

    /// Get the timestamp at which at least 70% of the signing power should be
    /// willing to accept a time-based tenure extension.
    pub fn get_tenure_extend_timestamp(&self) -> u64 {
        self.stackerdb_comms
            .get_tenure_extend_timestamp(self.weight_threshold)
    }

    /// Check if the tenure needs to change
    fn check_burn_tip_changed(sortdb: &SortitionDB, burn_block: &BlockSnapshot) -> bool {
        let cur_burn_chain_tip = SortitionDB::get_canonical_burn_chain_tip(sortdb.conn())
            .expect("FATAL: failed to query sortition DB for canonical burn chain tip");

        if cur_burn_chain_tip.consensus_hash != burn_block.consensus_hash {
            info!("SignerCoordinator: Cancel signature aggregation; burnchain tip has changed");
            true
        } else {
            false
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(listener_thread) = self.listener_thread.take() {
            info!("SignerCoordinator: shutting down stacker db listener thread");
            self.keep_running
                .store(false, std::sync::atomic::Ordering::Relaxed);
            if let Err(e) = listener_thread.join() {
                error!("Failed to join signer listener thread: {e:?}");
            }
            debug!("SignerCoordinator: stacker db listener thread has shut down");
        }
    }
}
