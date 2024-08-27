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

#[derive(Debug)]
pub struct StateSpaceManagerExEx {
    state: Arc<RwLock<StateSpace>>,
    _latest_synced_block: u64,
    state_change_cache: Arc<RwLock<StateChangeCache>>,
}

impl StateSpaceManagerExEx {
    pub fn new(amms: Vec<AMM>, latest_synced_block: u64) -> Self {
        Self {
            state: Arc::new(RwLock::new(amms.into())),
            _latest_synced_block: latest_synced_block,
            state_change_cache: Arc::new(RwLock::new(StateChangeCache::new())),
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

        self.state_change_cache
            .write()
            .await
            .unwind_state_changes(first_block - 1);

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
        let mut prev_state = vec![];

        for log in logs.into_iter() {
            // check if the log is from an amm in the state space
            if let Some(amm) = state.get_mut(&log.address) {
                updated_amms.insert(log.address);

                let rpc_log = alloy::rpc::types::eth::Log {
                    inner: log.clone(),
                    ..Default::default()
                };

                prev_state.push(amm.clone());
                amm.sync_from_log(rpc_log)?;
            }
        }

        if !prev_state.is_empty() {
            let state_change = StateChange::new(prev_state, block_number);

            self.state_change_cache
                .write()
                .await
                .add_state_change_to_cache(state_change);
        }

        Ok(updated_amms.into_iter().collect())
    }
}
