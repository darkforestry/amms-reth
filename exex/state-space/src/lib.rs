use amms::{
    amm::{AutomatedMarketMaker, AMM},
    state_space::error::StateChangeError,
};

use alloy::{primitives::Address, signers::k256::elliptic_curve::rand_core::block};

use amms::state_space::{StateChangeCache, StateSpace};
use arraydeque::ArrayDeque;
use reth_exex::ExExNotification;
use reth_primitives::Log;
use reth_provider::{ExecutionDataProvider, ExecutionOutcome};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::RwLock;

// TODO: Implement this directly into amms-rs, removing the option from state change
#[derive(Debug)]
pub struct StateChange {
    pub state_change: Vec<AMM>,
    pub block_number: u64,
}

impl StateChange {
    pub fn new(state_change: Vec<AMM>, block_number: u64) -> Self {
        Self {
            block_number,
            state_change,
        }
    }
}

#[derive(Debug)]
pub struct StateSpaceManagerExEx {
    state: Arc<RwLock<StateSpace>>,
    _latest_synced_block: u64,
    state_change_cache: Arc<RwLock<StateChangeCache>>,
}

impl StateSpaceManagerExEx {
    pub fn new(amms: Vec<AMM>, latest_synced_block: u64) -> Self {
        let state: HashMap<Address, AMM> = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<Address, AMM>>();

        Self {
            state: Arc::new(RwLock::new(state)),
            _latest_synced_block: latest_synced_block,
            state_change_cache: Arc::new(RwLock::new(ArrayDeque::new())),
        }
    }

    pub async fn process_notification(
        &self,
        notification: ExExNotification,
    ) -> Result<Vec<Address>, StateChangeError> {
        match notification {
            ExExNotification::ChainCommitted { new } => {
                let num_blocks = new.blocks().len() as u64;
                self.handle_state_changes(new.execution_outcome(), num_blocks)
                    .await
            }

            ExExNotification::ChainReorged { old: _old, new } => self.handle_reorgs(new).await,
            ExExNotification::ChainReverted { old: _old } => {
                unimplemented!("handle chain reverts");
            }
        }
    }

    pub async fn handle_reorgs(
        &self,
        new: Arc<reth_execution_types::Chain>,
    ) -> Result<Vec<Address>, StateChangeError> {
        // Unwind the state changes from the old state to the new state
        let first_block = new.first().number;
        // TODO: should this be - 1?
        self.unwind_state_changes(first_block - 1).await?;

        // Apply the new state changes
        let num_blocks = new.blocks().len() as u64;

        self.handle_state_changes(new.execution_outcome(), num_blocks)
            .await
    }

    pub async fn handle_state_changes(
        &self,
        execution_outcome: &ExecutionOutcome,
        num_blocks: u64,
    ) -> Result<Vec<Address>, StateChangeError> {
        let first_block = execution_outcome.first_block();

        let mut aggregated_updates = vec![];
        for block_number in first_block..first_block + num_blocks {
            if let Some(logs) = execution_outcome.logs(block_number) {
                let updated_amms = self.modify_state_from_logs(logs, block_number).await?;
                aggregated_updates.extend_from_slice(&updated_amms);
            }
        }

        Ok(aggregated_updates)
    }

    async fn modify_state_from_logs(
        &self,
        logs: impl Iterator<Item = &Log>,
        block_number: u64,
    ) -> Result<Vec<Address>, StateChangeError> {
        let mut updated_amms = HashSet::new();

        let mut state = self.state.write().await;

        // NOTE: we need to group the logs by block number before calling this function but we should assume that all logs are
        let mut state_changes = vec![];

        // TODO: collect state changes for the block and then add them
        for log in logs.into_iter() {
            // check if the log is from an amm in the state space
            if let Some(amm) = state.get_mut(&log.address) {
                if !updated_amms.contains(&log.address) {
                    updated_amms.insert(log.address);
                }

                let rpc_log = alloy::rpc::types::eth::Log {
                    inner: log.clone(),
                    ..Default::default()
                };

                state_changes.push(amm.clone());
                amm.sync_from_log(rpc_log)?;
            }
        }

        if !state_changes.is_empty() {
            let mut state_change_cache = self.state_change_cache.write().await;

            self.add_state_change_to_cache(
                *state_change_cache,
                StateChange::new(state_changes, block_number),
            )?;
        }

        Ok(updated_amms.into_iter().collect())
    }

    // TODO: implement this directly as trait method on StateChangeCache
    fn add_state_change_to_cache<const CAP: usize>(
        &self,
        mut state_change_cache: ArrayDeque<StateChange, CAP>,
        state_change: StateChange,
    ) -> Result<(), StateChangeError> {
        if state_change_cache.is_full() {
            state_change_cache.pop_back();
            state_change_cache
                .push_front(state_change)
                .map_err(|_| StateChangeError::CapacityError)?
        } else {
            state_change_cache
                .push_front(state_change)
                .map_err(|_| StateChangeError::CapacityError)?
        }
        Ok(())
    }

    /// Unwinds the state changes cache for every block from the most recent state change cache back to the block to unwind -1.

    // TODO: implement this directly into amms-rs. Additonally this can be more efficient, only storing and unwinding up to block_to_unwind rather than unwinding blocks with None
    async fn unwind_state_changes(&self, block_to_unwind: u64) -> Result<(), StateChangeError> {
        let mut state_change_cache = self.state_change_cache.write().await;

        // TODO: We can write the state change cache more efficiently
        loop {
            // Acquire a lock on state while unwinding state changes
            let mut state = self.state.write().await;
            // check if the most recent state change block is >= the block to unwind,
            if let Some(state_change) = state_change_cache.get(0) {
                if state_change.block_number >= block_to_unwind {
                    if let Some(option_state_changes) = state_change_cache.pop_front() {
                        if let Some(state_changes) = option_state_changes.state_change {
                            for amm_state in state_changes {
                                state.insert(amm_state.address(), amm_state);
                            }
                        }
                    } else {
                        // We know that there is a state change from state_change_cache.get(0) so when we pop front without returning a value, there is an issue
                        return Err(StateChangeError::PopFrontError);
                    }
                } else {
                    return Ok(());
                }
            } else {
                // We return an error here because we never want to be unwinding past where we have state changes.
                // For example, if you initialize a state space that syncs to block 100, then immediately after there is a chain reorg to 95, we can not roll back the state
                // changes for an accurate state space. In this case, we return an error
                return Err(StateChangeError::NoStateChangesInCache);
            }
        }
    }
}
