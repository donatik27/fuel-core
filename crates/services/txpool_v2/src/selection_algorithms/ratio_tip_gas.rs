use std::{
    cmp::{
        Ordering,
        Reverse,
    },
    collections::{
        BTreeMap,
        HashMap,
    },
    fmt::Debug,
    time::Instant,
};

use fuel_core_types::{
    fuel_tx::TxId,
    services::txpool::PoolTransaction,
};
use num_rational::Ratio;

use crate::{
    error::Error,
    storage::StorageData,
};

use super::{
    Constraints,
    SelectionAlgorithm,
};

pub trait RatioTipGasSelectionAlgorithmStorage {
    type StorageIndex: Copy + Debug;

    fn get(&self, index: &Self::StorageIndex) -> Result<&StorageData, Error>;
    fn get_dependents(
        &self,
        index: &Self::StorageIndex,
    ) -> Result<impl Iterator<Item = Self::StorageIndex>, Error>;
}

pub type RatioTipGas = Ratio<u64>;

/// Key used to sort transactions by tip/gas ratio.
/// It first compares the tip/gas ratio, then the creation instant and finally the transaction id.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub struct Key {
    ratio: RatioTipGas,
    creation_instant: Instant,
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
pub struct RatioTipGasSelection<S: RatioTipGasSelectionAlgorithmStorage> {
    executable_transactions_sorted_tip_gas_ratio: BTreeMap<Reverse<Key>, S::StorageIndex>,
    all_transactions_sorted_tip_gas_ratio: BTreeMap<Reverse<Key>, S::StorageIndex>,
    tx_id_to_creation_instant: HashMap<TxId, Instant>,
}

impl<S: RatioTipGasSelectionAlgorithmStorage> RatioTipGasSelection<S> {
    pub fn new() -> Self {
        Self {
            executable_transactions_sorted_tip_gas_ratio: BTreeMap::new(),
            all_transactions_sorted_tip_gas_ratio: BTreeMap::new(),
            tx_id_to_creation_instant: HashMap::new(),
        }
    }
}

impl<S: RatioTipGasSelectionAlgorithmStorage> Default for RatioTipGasSelection<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: RatioTipGasSelectionAlgorithmStorage> SelectionAlgorithm
    for RatioTipGasSelection<S>
{
    type Storage = S;
    type StorageIndex = S::StorageIndex;
    fn gather_best_txs(
        &mut self,
        constraints: Constraints,
        storage: &S,
    ) -> Result<Vec<S::StorageIndex>, Error> {
        let mut gas_left = constraints.max_gas;
        let mut best_transactions = Vec::new();

        // Take the first transaction with the highest tip/gas ratio if it fits in the gas limit
        // then promote all its dependents to the list of transactions to be executed
        // and repeat the process until the gas limit is reached
        while gas_left > 0
            && !self.executable_transactions_sorted_tip_gas_ratio.is_empty()
        {
            let mut new_executables = vec![];
            let mut best_transaction = None;

            let sorted_iter = self.executable_transactions_sorted_tip_gas_ratio.iter();
            for (key, storage_id) in sorted_iter {
                let enough_gas = {
                    let stored_transaction = storage.get(storage_id)?;
                    stored_transaction.transaction.max_gas() <= gas_left
                };
                if enough_gas {
                    new_executables.extend(storage.get_dependents(storage_id)?);
                    let stored_tx = storage.get(storage_id)?;
                    gas_left = gas_left.saturating_sub(stored_tx.transaction.max_gas());
                    best_transaction = Some((*key, *storage_id));
                    break;
                }
            }

            // Promote its dependents
            self.new_executable_transactions(new_executables, storage)?;
            // Remove the best transaction from the sorted list
            if let Some((key, best_transaction)) = best_transaction {
                self.executable_transactions_sorted_tip_gas_ratio
                    .remove(&key);
                best_transactions.push(best_transaction);
            } else {
                // If no transaction fits in the gas limit,
                // we can break the loop
                break;
            }
        }
        Ok(best_transactions)
    }

    fn new_executable_transactions(
        &mut self,
        transactions_ids: Vec<S::StorageIndex>,
        storage: &S,
    ) -> Result<(), Error> {
        for storage_id in transactions_ids {
            let stored_transaction = storage.get(&storage_id)?;
            let tip_gas_ratio = RatioTipGas::new(
                stored_transaction.transaction.tip(),
                stored_transaction.transaction.max_gas(),
            );
            let key = Key {
                ratio: tip_gas_ratio,
                creation_instant: stored_transaction.creation_instant,
                tx_id: stored_transaction.transaction.id(),
            };
            self.executable_transactions_sorted_tip_gas_ratio
                .insert(Reverse(key), storage_id);
            self.tx_id_to_creation_instant.insert(
                stored_transaction.transaction.id(),
                stored_transaction.creation_instant,
            );
        }
        Ok(())
    }

    fn get_less_worth_txs(&self) -> impl Iterator<Item = Self::StorageIndex> {
        self.all_transactions_sorted_tip_gas_ratio.values().copied()
    }

    fn on_stored_transaction(
        &mut self,
        transaction: &PoolTransaction,
        creation_instant: Instant,
        transaction_id: Self::StorageIndex,
    ) -> Result<(), Error> {
        let tip_gas_ratio = RatioTipGas::new(transaction.tip(), transaction.max_gas());
        let key = Key {
            ratio: tip_gas_ratio,
            creation_instant,
            tx_id: transaction.id(),
        };
        self.all_transactions_sorted_tip_gas_ratio
            .insert(Reverse(key), transaction_id);
        self.tx_id_to_creation_instant
            .insert(transaction.id(), creation_instant);
        Ok(())
    }

    fn on_removed_transaction(
        &mut self,
        transaction: &PoolTransaction,
    ) -> Result<(), Error> {
        let tip_gas_ratio = RatioTipGas::new(transaction.tip(), transaction.max_gas());
        let creation_instant = *self
            .tx_id_to_creation_instant
            .get(&transaction.id())
            .ok_or(Error::Storage(
                "Expected the transaction to be in the tx_id_to_creation_instant map"
                    .to_string(),
            ))?;
        let key = Key {
            ratio: tip_gas_ratio,
            creation_instant,
            tx_id: transaction.id(),
        };
        self.executable_transactions_sorted_tip_gas_ratio
            .remove(&Reverse(key));
        self.all_transactions_sorted_tip_gas_ratio
            .remove(&Reverse(key));
        self.tx_id_to_creation_instant.remove(&transaction.id());
        Ok(())
    }
}
