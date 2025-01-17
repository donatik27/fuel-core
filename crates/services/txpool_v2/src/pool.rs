mod collisions;

use std::{
    collections::HashMap,
    iter,
    time::Instant,
};

use fuel_core_types::{
    fuel_tx::{
        field::BlobId,
        TxId,
    },
    services::txpool::PoolTransaction,
};
use num_rational::Ratio;
use tracing::instrument;

use crate::{
    collision_manager::{
        CollisionManager,
        Collisions,
    },
    config::Config,
    error::Error,
    pool::collisions::CollisionsExt,
    ports::{
        AtomicView,
        TxPoolPersistentStorage,
    },
    selection_algorithms::{
        Constraints,
        SelectionAlgorithm,
    },
    storage::{
        CheckedTransaction,
        Storage,
        StorageData,
    },
};

/// The pool is the main component of the txpool service. It is responsible for storing transactions
/// and allowing the selection of transactions for inclusion in a block.
pub struct Pool<PSProvider, S, SI, CM, SA> {
    /// Configuration of the pool.
    pub(crate) config: Config,
    /// The storage of the pool.
    pub(crate) storage: S,
    /// The collision manager of the pool.
    pub(crate) collision_manager: CM,
    /// The selection algorithm of the pool.
    pub(crate) selection_algorithm: SA,
    /// The persistent storage of the pool.
    pub(crate) persistent_storage_provider: PSProvider,
    /// Mapping from tx_id to storage_id.
    pub(crate) tx_id_to_storage_id: HashMap<TxId, SI>,
    /// Current pool gas stored.
    pub(crate) current_gas: u64,
    /// Current pool size in bytes.
    pub(crate) current_bytes_size: usize,
}

impl<PSProvider, S, SI, CM, SA> Pool<PSProvider, S, SI, CM, SA> {
    /// Create a new pool.
    pub fn new(
        persistent_storage_provider: PSProvider,
        storage: S,
        collision_manager: CM,
        selection_algorithm: SA,
        config: Config,
    ) -> Self {
        Pool {
            storage,
            collision_manager,
            selection_algorithm,
            persistent_storage_provider,
            config,
            tx_id_to_storage_id: HashMap::new(),
            current_gas: 0,
            current_bytes_size: 0,
        }
    }
}

impl<PS, View, S: Storage, CM, SA> Pool<PS, S, S::StorageIndex, CM, SA>
where
    PS: AtomicView<LatestView = View>,
    View: TxPoolPersistentStorage,
    S: Storage,
    CM: CollisionManager<StorageIndex = S::StorageIndex>,
    SA: SelectionAlgorithm<Storage = S, StorageIndex = S::StorageIndex>,
{
    /// Insert transactions into the pool.
    /// Returns a list of results for each transaction.
    /// Each result is a list of transactions that were removed from the pool
    /// because of the insertion of the new transaction.
    #[instrument(skip(self))]
    pub fn insert(&mut self, tx: PoolTransaction) -> Result<Vec<PoolTransaction>, Error> {
        let CanStoreTransaction {
            checked_transaction,
            transactions_to_remove,
            collisions,
            _guard,
        } = self.can_insert_transaction(tx)?;

        let has_dependencies = !checked_transaction.all_dependencies().is_empty();

        let mut removed_transactions = vec![];
        for tx in transactions_to_remove {
            let removed = self.storage.remove_transaction_and_dependents_subtree(tx);
            self.update_components_and_caches_on_removal(removed.iter());
            removed_transactions.extend(removed);
        }

        for collided_tx in collisions.keys() {
            let removed = self
                .storage
                .remove_transaction_and_dependents_subtree(*collided_tx);
            self.update_components_and_caches_on_removal(removed.iter());

            removed_transactions.extend(removed);
        }

        let tx = checked_transaction.tx();
        let tx_id = tx.id();
        let gas = tx.max_gas();
        let creation_instant = Instant::now();
        let bytes_size = tx.metered_bytes_size();

        let storage_id = self
            .storage
            .store_transaction(checked_transaction, creation_instant);

        self.current_gas = self.current_gas.saturating_add(gas);
        self.current_bytes_size = self.current_bytes_size.saturating_add(bytes_size);
        self.tx_id_to_storage_id.insert(tx_id, storage_id);

        let tx =
            Storage::get(&self.storage, &storage_id).expect("Transaction is set above");
        self.collision_manager.on_stored_transaction(storage_id, tx);

        // No dependencies directly in the graph and the sorted transactions
        if !has_dependencies {
            self.selection_algorithm
                .new_executable_transaction(storage_id, tx);
        }

        let removed_transactions = removed_transactions
            .into_iter()
            .map(|data| data.transaction)
            .collect::<Vec<_>>();

        Ok(removed_transactions)
    }

    /// Check if a transaction can be inserted into the pool.
    #[allow(clippy::type_complexity)]
    pub fn can_insert_transaction(
        &self,
        tx: PoolTransaction,
    ) -> Result<CanStoreTransaction<S>, Error> {
        let persistent_storage = self
            .persistent_storage_provider
            .latest_view()
            .map_err(|e| Error::Database(format!("{:?}", e)))?;

        if tx.max_gas() == 0 {
            return Err(Error::ZeroMaxGas)
        }

        let tx_id = tx.id();
        if self.tx_id_to_storage_id.contains_key(&tx_id) {
            return Err(Error::DuplicateTxId(tx_id))
        }

        self.config.black_list.check_blacklisting(&tx)?;

        Self::check_blob_does_not_exist(&tx, &persistent_storage)?;
        self.storage.validate_inputs(
            &tx,
            &persistent_storage,
            self.config.utxo_validation,
        )?;

        let collisions = self.collision_manager.find_collisions(&tx)?;
        let checked_transaction = self.storage.can_store_transaction(tx)?;

        for collision in collisions.keys() {
            if checked_transaction.all_dependencies().contains(collision) {
                return Err(Error::NotInsertedCollisionIsDependency);
            }
        }

        let has_dependencies = !checked_transaction.all_dependencies().is_empty();

        collisions
            .check_collision_requirements(
                checked_transaction.tx(),
                has_dependencies,
                &self.storage,
            )
            .map_err(Error::Collided)?;

        let can_fit_into_pool = self.can_fit_into_pool(&checked_transaction)?;

        let mut transactions_to_remove = vec![];
        if let SpaceCheckResult::NotEnoughSpace(left) = can_fit_into_pool {
            transactions_to_remove = self.find_free_space(left, &checked_transaction)?;
        }

        let can_store_transaction = CanStoreTransaction {
            checked_transaction,
            transactions_to_remove,
            collisions,
            _guard: &self.storage,
        };

        Ok(can_store_transaction)
    }

    // TODO: Use block space also (https://github.com/FuelLabs/fuel-core/issues/2133)
    /// Extract transactions for a block.
    /// Returns a list of transactions that were selected for the block
    /// based on the constraints given in the configuration and the selection algorithm used.
    pub fn extract_transactions_for_block(
        &mut self,
    ) -> Result<Vec<PoolTransaction>, Error> {
        let extracted_transactions = self
            .selection_algorithm
            .gather_best_txs(
                Constraints {
                    max_gas: self.config.max_block_gas,
                },
                &mut self.storage,
            )?
            .into_iter()
            .map(|storage_entry| {
                self.update_components_and_caches_on_removal(iter::once(&storage_entry));

                storage_entry.transaction
            })
            .collect::<Vec<_>>();

        Ok(extracted_transactions)
    }

    #[allow(dead_code)]
    pub fn find_one(&self, tx_id: &TxId) -> Option<&PoolTransaction> {
        Storage::get(&self.storage, self.tx_id_to_storage_id.get(tx_id)?)
            .map(|data| &data.transaction)
    }

    /// Check if the pool has enough space to store a transaction.
    ///
    /// It returns `true` if:
    /// - Pool is not full
    /// - Removing colliding subtree is enough to make space
    ///
    /// It returns an error if the pool is full and transactions has dependencies.
    ///
    /// If none of the above is not true, return `false`.
    fn can_fit_into_pool(
        &self,
        checked_transaction: &S::CheckedTransaction,
    ) -> Result<SpaceCheckResult, Error> {
        let tx = checked_transaction.tx();
        let tx_gas = tx.max_gas();
        let bytes_size = tx.metered_bytes_size();
        let gas_left = self.current_gas.saturating_add(tx_gas);
        let bytes_left = self.current_bytes_size.saturating_add(bytes_size);
        let txs_left = self.tx_id_to_storage_id.len().saturating_add(1);
        if gas_left <= self.config.pool_limits.max_gas
            && bytes_left <= self.config.pool_limits.max_bytes_size
            && txs_left <= self.config.pool_limits.max_txs
        {
            return Ok(SpaceCheckResult::EnoughSpace);
        }

        let has_dependencies = !checked_transaction.all_dependencies().is_empty();

        // If the transaction has a dependency and the pool is full, we refuse it
        if has_dependencies {
            return Err(Error::NotInsertedLimitHit);
        }

        let left = NotEnoughSpace {
            gas_left,
            bytes_left,
            txs_left,
        };

        Ok(SpaceCheckResult::NotEnoughSpace(left))
    }

    /// Find free space in the pool by marking less profitable transactions for removal.
    ///
    /// Return the list of transactions that must be removed from the pool along all of
    /// their dependent subtree.
    ///
    /// Returns an error impossible to find enough space.
    fn find_free_space(
        &self,
        left: NotEnoughSpace,
        checked_transaction: &S::CheckedTransaction,
    ) -> Result<Vec<S::StorageIndex>, Error> {
        let tx = checked_transaction.tx();
        let NotEnoughSpace {
            mut gas_left,
            mut bytes_left,
            mut txs_left,
        } = left;

        // Here the transaction has no dependencies which means that it's an executable transaction
        // and we want to make space for it
        let new_tx_ratio = Ratio::new(tx.tip(), tx.max_gas());

        // We want to go over executable transactions and remove the less profitable ones.
        // It is imported to go over executable transactions, not dependent ones, because we
        // can include the same subtree several times in the calculation if we use dependent txs.
        let mut sorted_txs = self.selection_algorithm.get_less_worth_txs();

        let mut transactions_to_remove = vec![];

        while gas_left > self.config.pool_limits.max_gas
            || bytes_left > self.config.pool_limits.max_bytes_size
            || txs_left > self.config.pool_limits.max_txs
        {
            let storage_id = sorted_txs.next().ok_or(Error::NotInsertedLimitHit)?;

            if checked_transaction.all_dependencies().contains(storage_id) {
                continue
            }

            debug_assert!(!self.storage.has_dependencies(storage_id));

            let Some(storage_data) = self.storage.get(storage_id) else {
                debug_assert!(
                    false,
                    "Storage data not found for one of the less worth transactions"
                );
                tracing::warn!(
                    "Storage data not found for one of the less \
                    worth transactions during `find_free_space`."
                );
                continue
            };
            let ratio = Ratio::new(
                storage_data.dependents_cumulative_tip,
                storage_data.dependents_cumulative_gas,
            );

            if ratio > new_tx_ratio {
                return Err(Error::NotInsertedLimitHit);
            }

            // It is not a correct way of deciding how many gas and bytes we will free
            // by removing this transaction, but the fastest way.
            // It will return incorrect values if several transactions have the same
            // dependents. An example:
            //
            // E - Executable transaction
            // D - Dependent transaction
            //
            //   E   E
            //    \ /
            //     D
            //    / \
            //   D   D
            //
            // Removing both E frees (E + E + 3D), while this loop assumes 2 * (E + 3D).
            // But it is okay since the limits for the TxPool are not strict.
            let gas = storage_data.dependents_cumulative_gas;
            let bytes = storage_data.dependents_cumulative_bytes_size;
            let txs = storage_data.number_dependents_in_chain;

            gas_left = gas_left.saturating_sub(gas);
            bytes_left = bytes_left.saturating_sub(bytes);
            txs_left = txs_left.saturating_sub(txs);

            transactions_to_remove.push(*storage_id);
        }

        Ok(transactions_to_remove)
    }

    fn check_blob_does_not_exist(
        tx: &PoolTransaction,
        persistent_storage: &impl TxPoolPersistentStorage,
    ) -> Result<(), Error> {
        if let PoolTransaction::Blob(checked_tx, _) = &tx {
            let blob_id = checked_tx.transaction().blob_id();
            if persistent_storage
                .blob_exist(blob_id)
                .map_err(|e| Error::Database(format!("{:?}", e)))?
            {
                return Err(Error::NotInsertedBlobIdAlreadyTaken(*blob_id))
            }
        }
        Ok(())
    }

    fn update_components_and_caches_on_removal<'a>(
        &mut self,
        removed_transactions: impl Iterator<Item = &'a StorageData>,
    ) {
        for storage_entry in removed_transactions {
            let tx = &storage_entry.transaction;
            self.current_gas = self.current_gas.saturating_sub(tx.max_gas());
            self.current_bytes_size = self
                .current_bytes_size
                .saturating_sub(tx.metered_bytes_size());
            self.tx_id_to_storage_id.remove(&tx.id());
            self.collision_manager.on_removed_transaction(tx);
            self.selection_algorithm
                .on_removed_transaction(storage_entry);
        }
    }
}

pub struct NotEnoughSpace {
    gas_left: u64,
    bytes_left: usize,
    txs_left: usize,
}

/// The result of the `can_fit_into_pool` check.
pub enum SpaceCheckResult {
    EnoughSpace,
    NotEnoughSpace(NotEnoughSpace),
}

pub struct CanStoreTransaction<'a, S>
where
    S: Storage,
{
    /// The checked transaction by the storage.
    checked_transaction: S::CheckedTransaction,
    /// List of transactions to remove to fit the new transaction into the pool.
    transactions_to_remove: Vec<S::StorageIndex>,
    /// List of collided transactions that we need to remove to insert transaction.
    collisions: Collisions<S::StorageIndex>,
    /// Protects the pool from modifications while this type is active.
    _guard: &'a S,
}

impl<'a, S> CanStoreTransaction<'a, S>
where
    S: Storage,
{
    pub fn into_transaction(self) -> PoolTransaction {
        self.checked_transaction.into_tx()
    }
}
