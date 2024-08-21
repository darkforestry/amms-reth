use amms::{
    amm::{AutomatedMarketMaker, AMM},
    state_space::error::StateChangeError,
};

use alloy::{
    primitives::{Address, B256},
    rpc::types::eth::Filter,
};

use amms::state_space::{StateChangeCache, StateSpace};
use arraydeque::ArrayDeque;
use reth_exex::ExExNotification;
use reth_node_api::FullNodeComponents;
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
}

impl StateSpaceManagerExEx {
    pub fn new(amms: Vec<AMM>) -> Self {
        let state: HashMap<Address, AMM> = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<Address, AMM>>();

        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }

    pub async fn process_notification(
        &self,
        notification: ExExNotification,
    ) -> Result<Vec<Address>, StateChangeError> {
        match notification {
            ExExNotification::ChainCommitted { new } => {
                let execuiton_outcome = new.execution_outcome();
                self.handle_state_changes(execuiton_outcome).await
            }

            ExExNotification::ChainReorged { old: _old, new } => {
                let execuiton_outcome = new.execution_outcome();
                self.handle_state_changes(execuiton_outcome).await
            }
            ExExNotification::ChainReverted { old: _old } => {
                unimplemented!("handle chain reverts");
            }
        }
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
        for log in logs.into_iter() {
            // check if the log is from an amm in the state space
            if let Some(amm) = self.state.write().await.get_mut(&log.address) {
                if !updated_amms.contains(&log.address) {
                    updated_amms.insert(log.address);
                }

                let rpc_log = alloy::rpc::types::eth::Log {
                    inner: log.clone(),
                    ..Default::default()
                };

                amm.sync_from_log(rpc_log)?;
            }
        }

        Ok(updated_amms.into_iter().collect())
    }
}
