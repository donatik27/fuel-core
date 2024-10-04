use std::{
    cmp::{
        Ordering,
        Reverse,
    },
    collections::BTreeMap,
    fmt::Debug,
    ops::Deref,
    sync::Arc,
    time::SystemTime,
};

use fuel_core_types::{
    fuel_tx::{
        field::MaxFeeLimit,
        FeeParameters,
        GasCosts,
        TransactionFee,
        TxId,
    },
    services::txpool::{
        ArcPoolTx,
        PoolTransaction,
    },
};
use num_rational::Ratio;

use crate::{
    error::Error,
    ports::{
        ConsensusParametersProvider,
        GasPriceProvider,
    },
    storage::{
        RemovedTransactions,
        StorageData,
    },
    GasPrice,
};

use super::{
    Constraints,
    SelectionAlgorithm,
};

pub trait RatioTipGasSelectionAlgorithmStorage {
    type StorageIndex: Debug;

    fn get(&self, index: &Self::StorageIndex) -> Option<&StorageData>;

    fn get_dependents(
        &self,
        index: &Self::StorageIndex,
    ) -> impl Iterator<Item = Self::StorageIndex>;

    fn has_dependencies(&self, index: &Self::StorageIndex) -> bool;

    fn remove(&mut self, index: &Self::StorageIndex) -> Option<StorageData>;
}

pub type RatioTipGas = Ratio<u64>;

/// Key used to sort transactions by tip/gas ratio.
/// It first compares the tip/gas ratio, then the creation instant and finally the transaction id.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub struct Key {
    ratio: RatioTipGas,
    creation_instant: SystemTime,
    tx_id: TxId,
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        let cmp = self.ratio.cmp(&other.ratio);
        if cmp == Ordering::Equal {
            let instant_cmp = other.creation_instant.cmp(&self.creation_instant);
            if instant_cmp == Ordering::Equal {
                self.tx_id.cmp(&other.tx_id)
            } else {
                instant_cmp
            }
        } else {
            cmp
        }
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The selection algorithm that selects transactions based on the tip/gas ratio.
pub struct RatioTipGasSelection<S: RatioTipGasSelectionAlgorithmStorage, G, C> {
    executable_transactions_sorted_tip_gas_ratio: BTreeMap<Reverse<Key>, S::StorageIndex>,
    gas_price_provider: Arc<G>,
    consensus_params_provider: Arc<C>,
}

impl<S: RatioTipGasSelectionAlgorithmStorage, G, C> RatioTipGasSelection<S, G, C> {
    pub fn new(gas_price_provider: Arc<G>, consensus_params_provider: Arc<C>) -> Self {
        Self {
            executable_transactions_sorted_tip_gas_ratio: BTreeMap::new(),
            gas_price_provider,
            consensus_params_provider,
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.executable_transactions_sorted_tip_gas_ratio.is_empty()
    }

    fn key(store_entry: &StorageData) -> Key {
        let transaction = &store_entry.transaction;
        let tip_gas_ratio = RatioTipGas::new(transaction.tip(), transaction.max_gas());

        Key {
            ratio: tip_gas_ratio,
            creation_instant: store_entry.creation_instant,
            tx_id: transaction.id(),
        }
    }

    fn on_removed_transaction_inner(&mut self, key: Key) {
        self.executable_transactions_sorted_tip_gas_ratio
            .remove(&Reverse(key));
    }
}

impl<
        S: RatioTipGasSelectionAlgorithmStorage,
        G: GasPriceProvider,
        C: ConsensusParametersProvider,
    > SelectionAlgorithm for RatioTipGasSelection<S, G, C>
{
    type Storage = S;
    type StorageIndex = S::StorageIndex;

    fn gather_best_txs(
        &mut self,
        constraints: Constraints,
        storage: &mut S,
    ) -> Result<RemovedTransactions, Error> {
        let mut gas_left = constraints.max_gas;
        let mut space_left = constraints.maximum_block_size as usize;
        let mut nb_left = constraints.maximum_txs;
        let gas_price =
            { futures::executor::block_on(self.gas_price_provider.next_gas_price()) }?;
        let consensus_params = self
            .consensus_params_provider
            .latest_consensus_parameters()
            .1;
        let gas_costs = consensus_params.gas_costs();
        let fee_params = consensus_params.fee_params();
        let mut result = Vec::new();

        // Take iterate over all transactions with the highest tip/gas ratio. If transaction
        // fits in the gas limit select it and mark all its dependents to be promoted.
        // Do that until end of the list or gas limit is reached. If gas limit is not
        // reached, but we have promoted transactions we can start again from the beginning.
        // Otherwise, we can break the loop.
        // It is done in this way to minimize number of iteration of the list of executable
        // transactions.
        while gas_left > 0
            && nb_left > 0
            && space_left > 0
            && !self.executable_transactions_sorted_tip_gas_ratio.is_empty()
        {
            let mut clean_up_list = Vec::new();
            let mut transactions_to_remove = Vec::new();
            let mut transactions_to_promote = Vec::new();

            for (key, storage_id) in &self.executable_transactions_sorted_tip_gas_ratio {
                let Some(stored_transaction) = storage.get(storage_id) else {
                    debug_assert!(
                        false,
                        "Transaction not found in the storage during `gather_best_txs`."
                    );
                    tracing::warn!(
                        "Transaction not found in the storage during `gather_best_txs`."
                    );
                    transactions_to_remove.push(*key);
                    continue
                };

                if !tx_is_gas_price_valid(
                    &stored_transaction.transaction,
                    gas_costs,
                    fee_params,
                    gas_price,
                ) {
                    continue;
                }
                if stored_transaction.transaction.max_gas() > gas_left
                    || stored_transaction.transaction.metered_bytes_size() > space_left
                {
                    continue;
                }

                gas_left =
                    gas_left.saturating_sub(stored_transaction.transaction.max_gas());
                space_left = space_left
                    .saturating_sub(stored_transaction.transaction.metered_bytes_size());
                nb_left = nb_left.saturating_sub(1);

                let dependents = storage.get_dependents(storage_id).collect::<Vec<_>>();
                debug_assert!(!storage.has_dependencies(storage_id));
                let removed = storage.remove(storage_id).expect(
                    "We just get the transaction from the storage above, it should exist.",
                );
                clean_up_list.push(*key);
                result.push(removed);

                for dependent in dependents {
                    if !storage.has_dependencies(&dependent) {
                        transactions_to_promote.push(dependent);
                    }
                }
            }

            for remove in transactions_to_remove {
                let key = remove.0;
                self.on_removed_transaction_inner(key);
            }

            // If no transaction fits in the gas limit and no one to promote, we can break the loop
            if clean_up_list.is_empty() && transactions_to_promote.is_empty() {
                break;
            }

            for key in clean_up_list {
                let key = key.0;
                // Remove selected transactions from the sorted list
                self.on_removed_transaction_inner(key);
            }

            for promote in transactions_to_promote {
                let storage = storage.get(&promote).expect(
                    "We just get the dependent from the storage, it should exist.",
                );

                self.new_executable_transaction(promote, storage);
            }
        }

        Ok(result)
    }

    fn new_executable_transaction(
        &mut self,
        storage_id: Self::StorageIndex,
        store_entry: &StorageData,
    ) {
        let key = Self::key(store_entry);
        self.executable_transactions_sorted_tip_gas_ratio
            .insert(Reverse(key), storage_id);
    }

    fn get_less_worth_txs(&self) -> impl Iterator<Item = &Self::StorageIndex> {
        self.executable_transactions_sorted_tip_gas_ratio
            .values()
            .rev()
    }

    fn on_removed_transaction(&mut self, storage_entry: &StorageData) {
        let key = Self::key(storage_entry);
        self.on_removed_transaction_inner(key)
    }
}

fn tx_is_gas_price_valid(
    tx: &ArcPoolTx,
    gas_costs: &GasCosts,
    fee_params: &FeeParameters,
    gas_price: GasPrice,
) -> bool {
    match tx.deref() {
        PoolTransaction::Upgrade(tx, _) => {
            let Some(tx_fees) = TransactionFee::checked_from_tx(
                gas_costs,
                fee_params,
                tx.transaction(),
                gas_price,
            ) else {
                return false;
            };
            let max_fee_from_policies = tx.transaction().max_fee_limit();
            let max_fee_from_gas_price = tx_fees.max_fee();
            max_fee_from_gas_price <= max_fee_from_policies
        }
        PoolTransaction::Create(tx, _) => {
            let Some(tx_fees) = TransactionFee::checked_from_tx(
                gas_costs,
                fee_params,
                tx.transaction(),
                gas_price,
            ) else {
                return false;
            };
            let max_fee_from_policies = tx.transaction().max_fee_limit();
            let max_fee_from_gas_price = tx_fees.max_fee();
            max_fee_from_gas_price <= max_fee_from_policies
        }
        PoolTransaction::Script(tx, _) => {
            let Some(tx_fees) = TransactionFee::checked_from_tx(
                gas_costs,
                fee_params,
                tx.transaction(),
                gas_price,
            ) else {
                return false;
            };
            let max_fee_from_policies = tx.transaction().max_fee_limit();
            let max_fee_from_gas_price = tx_fees.max_fee();
            max_fee_from_gas_price <= max_fee_from_policies
        }
        PoolTransaction::Upload(tx, _) => {
            let Some(tx_fees) = TransactionFee::checked_from_tx(
                gas_costs,
                fee_params,
                tx.transaction(),
                gas_price,
            ) else {
                return false;
            };
            let max_fee_from_policies = tx.transaction().max_fee_limit();
            let max_fee_from_gas_price = tx_fees.max_fee();
            max_fee_from_gas_price <= max_fee_from_policies
        }
        PoolTransaction::Blob(tx, _) => {
            let Some(tx_fees) = TransactionFee::checked_from_tx(
                gas_costs,
                fee_params,
                tx.transaction(),
                gas_price,
            ) else {
                return false;
            };
            let max_fee_from_policies = tx.transaction().max_fee_limit();
            let max_fee_from_gas_price = tx_fees.max_fee();
            max_fee_from_gas_price <= max_fee_from_policies
        }
    }
}
