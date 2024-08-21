use amms::{
    amm::{AutomatedMarketMaker, AMM},
    state_space::{error::StateChangeError, StateChange},
};

use alloy::primitives::Address;

use amms::state_space::{StateChangeCache, StateSpace};
use arraydeque::ArrayDeque;
use reth_exex::ExExNotification;
use reth_primitives::Log;
use reth_provider::ExecutionOutcome;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::RwLock;

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
        // TODO: return addresses affected by state changes
        match notification {
            ExExNotification::ChainCommitted { new } => {
                self.handle_state_changes(new.execution_outcome()).await
            }

            ExExNotification::ChainReorged { old: _old, new } => {
                self.handle_reorgs(new.execution_outcome()).await
            }
            ExExNotification::ChainReverted { old: _old } => {
                unimplemented!("handle chain reverts");
            }
        }
    }

    pub async fn handle_reorgs(
        &self,
        new: &ExecutionOutcome,
    ) -> Result<Vec<Address>, StateChangeError> {
        // Unwind the state changes from the old state to the new state

        // TODO: should this be - 1?
        self.unwind_state_changes(new.first_block - 1).await?;

        // Apply the new state changes
        self.handle_state_changes(new).await
    }

    pub async fn handle_state_changes(
        &self,
        execution_outcome: &ExecutionOutcome,
    ) -> Result<Vec<Address>, StateChangeError> {
        let block_number = execution_outcome.first_block();

        let logs = (block_number
            ..=(block_number + execution_outcome.receipts().receipt_vec.len() as u64 - 1))
            .filter_map(|block_number| execution_outcome.logs(block_number))
            .flatten()
            .cloned()
            .collect::<Vec<Log>>();

        self.modify_state_from_logs(logs).await
    }

    async fn modify_state_from_logs(
        &self,
        logs: Vec<Log>,
    ) -> Result<Vec<Address>, StateChangeError> {
        let mut updated_amms = HashSet::new();

        let mut state = self.state.write().await;
        let mut state_change_cache = self.state_change_cache.write().await;
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

                amm.sync_from_log(rpc_log)?;

                // TODO:
                // self.add_state_change_to_cache(*state_change_cache, StateChange {});
            }
        }

        Ok(updated_amms.into_iter().collect())
    }

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
