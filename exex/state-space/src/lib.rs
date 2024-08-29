use amms::{
    amm::{AutomatedMarketMaker, AMM},
    state_space::{cache::StateChangeCache, error::StateSpaceError, StateChange},
};

use alloy::primitives::Address;
use amms::state_space::StateSpace;
use reth_exex::ExExNotification;
use reth_primitives::Log;
use reth_provider::ExecutionOutcome;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::RwLock;

#[derive(Debug)]
pub struct StateSpaceManagerExEx {
    state: Arc<RwLock<StateSpace>>,
    state_change_cache: StateChangeCache,
}

impl StateSpaceManagerExEx {
    pub fn new(amms: Vec<AMM>) -> Self {
        Self {
            state: Arc::new(RwLock::new(amms.into())),
            state_change_cache: StateChangeCache::new(),
        }
    }

    pub async fn process_notification(
        &mut self,
        notification: ExExNotification,
    ) -> Result<Vec<Address>, StateSpaceError> {
        match notification {
            ExExNotification::ChainCommitted { new } => {
                let num_blocks = new.blocks().len() as u64;
                self.handle_state_changes(new.execution_outcome(), num_blocks)
                    .await
            }

            ExExNotification::ChainReorged { old: _, new } => {
                let first_block = new.first().number;
                let num_blocks = new.blocks().len() as u64;

                self.state_change_cache
                    .unwind_state_changes(first_block - 1);

                self.handle_state_changes(new.execution_outcome(), num_blocks)
                    .await
            }
            ExExNotification::ChainReverted { old } => {
                let first_block = old.first().number;
                let num_blocks = old.blocks().len() as u64;

                let amms = self
                    .state_change_cache
                    .unwind_state_changes(first_block + num_blocks - 1);

                let addresses = amms.iter().map(|amm| amm.address()).collect();

                Ok(addresses)
            }
        }
    }

    pub async fn handle_state_changes(
        &mut self,
        execution_outcome: &ExecutionOutcome,
        num_blocks: u64,
    ) -> Result<Vec<Address>, StateSpaceError> {
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
        &mut self,
        logs: impl Iterator<Item = &Log>,
        block_number: u64,
    ) -> Result<Vec<Address>, StateSpaceError> {
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
                .add_state_change_to_cache(state_change)?;
        }

        Ok(updated_amms.into_iter().collect())
    }
}
