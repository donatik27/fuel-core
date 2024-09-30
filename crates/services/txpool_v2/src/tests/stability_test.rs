#![allow(non_snake_case)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::arithmetic_side_effects)]

use crate::{
    config::{
        Config,
        PoolLimits,
    },
    tests::context::TestPoolUniverse,
};
use fuel_core_types::{
    fuel_asm::{
        op,
        RegId,
    },
    fuel_crypto::SecretKey,
    fuel_tx::{
        field::Tip,
        ConsensusParameters,
        Finalizable,
        GasCosts,
        Input,
        Output,
        Script,
        TransactionBuilder,
        TxId,
        UtxoId,
    },
    fuel_types::AssetId,
    fuel_vm::{
        checked_transaction::{
            Checked,
            EstimatePredicates,
            IntoChecked,
        },
        interpreter::MemoryInstance,
    },
    services::txpool::{
        Metadata,
        PoolTransaction,
    },
};
use rand::{
    prelude::SliceRandom,
    rngs::StdRng,
    Rng,
    SeedableRng,
};
use std::{
    collections::HashSet,
    time::Instant,
};

#[derive(Debug, Clone, Copy)]
struct Limits {
    max_inputs: usize,
    min_outputs: u16,
    max_outputs: u16,
    utxo_id_range: u8,
    gas_limit_range: u64,
}

fn some_transaction(
    limits: Limits,
    tip: u64,
    rng: &mut StdRng,
) -> (Checked<Script>, Metadata) {
    const AMOUNT: u64 = 10000;

    let mut consensus_parameters = ConsensusParameters::standard();
    consensus_parameters.set_gas_costs(GasCosts::free());

    let mut builder = TransactionBuilder::script(vec![], vec![]);
    builder.with_params(consensus_parameters.clone());

    let mut owner = [0u8; 32];
    owner[0] = 123;
    owner[1] = 222;
    let owner = owner.into();

    let mut random_ids = (0..limits.utxo_id_range)
        .map(|_| rng.gen_range(0..limits.utxo_id_range))
        .collect::<Vec<_>>();
    random_ids.sort();
    random_ids.dedup();
    random_ids.shuffle(rng);

    let tx_id_byte = random_ids.pop().expect("No random ids");
    let tx_id: TxId = [tx_id_byte; 32].into();
    let max_gas = rng.gen_range(0..limits.gas_limit_range);

    let inputs_count = limits.max_inputs.min(random_ids.len());
    let inputs_count = rng.gen_range(1..inputs_count);
    let inputs = random_ids.into_iter().take(inputs_count);

    for input_utxo_id in inputs {
        let output_index = rng.gen_range(0..limits.max_outputs);
        let utxo_id = UtxoId::new([input_utxo_id; 32].into(), output_index);

        builder.add_input(Input::coin_signed(
            utxo_id,
            owner,
            AMOUNT,
            AssetId::BASE,
            Default::default(),
            Default::default(),
        ));
    }

    // We can't have more outputs than inputs.
    let outputs = rng
        .gen_range(limits.min_outputs..limits.max_outputs)
        .min(inputs_count as u16);

    for _ in 0..outputs {
        let output = Output::coin(owner, AMOUNT, AssetId::BASE);
        builder.add_output(output);
    }

    builder.add_witness(Default::default());

    let mut tx = builder.finalize();
    tx.set_tip(tip);

    let checked = tx
        .into_checked_basic(0u32.into(), &consensus_parameters)
        .unwrap();
    let metadata = Metadata::new_test(0, Some(max_gas), Some(tx_id));

    (checked, metadata)
}

fn stability_test(limits: Limits, config: Config) {
    use rand::RngCore;
    let seed = rand::thread_rng().next_u64();

    let result = std::panic::catch_unwind(|| {
        stability_test_with_seed(seed, limits, config);
    });

    if let Err(err) = result {
        tracing::error!("Stability test failed with seed: {}; err: {:?}", seed, err);
        panic!("Stability test failed with seed: {}; err: {:?}", seed, err);
    }
}

fn stability_test_with_seed(seed: u64, limits: Limits, config: Config) {
    let mut rng = StdRng::seed_from_u64(seed);

    let mut errors = 0;

    let txpool = TestPoolUniverse::default().config(config).build_pool();
    let mut txpool = txpool.write();

    for tip in 0..ROUNDS_PER_TXPOOL {
        let (checked, metadata) = some_transaction(limits, tip as u64, &mut rng);
        let pool_tx = PoolTransaction::Script(checked, metadata);

        let result = txpool.insert(pool_tx);
        errors += result.is_err() as usize;
    }

    assert_ne!(ROUNDS_PER_TXPOOL, errors);

    loop {
        let result = txpool.extract_transactions_for_block().unwrap();

        if result.is_empty() {
            break
        }
    }

    assert_eq!(txpool.current_gas, 0);
    assert_eq!(txpool.current_bytes_size, 0);
    assert!(txpool.tx_id_to_storage_id.is_empty());
    assert!(txpool.selection_algorithm.is_empty());
    assert!(txpool.storage.is_empty());
    assert!(txpool.collision_manager.is_empty());
}

const ROUNDS_PER_TEST: usize = 30;
const ROUNDS_PER_TXPOOL: usize = 1500;

#[test]
fn stability_test__average_transactions() {
    let config = Config {
        utxo_validation: false,
        ..Default::default()
    };

    let limit = Limits {
        max_inputs: 4,
        min_outputs: 1,
        max_outputs: 4,
        utxo_id_range: 12,
        gas_limit_range: 1000,
    };

    for _ in 0..ROUNDS_PER_TEST {
        stability_test(limit, config.clone());
    }
}

#[test]
fn stability_test__many_non_conflicting_dependencies() {
    let config = Config {
        utxo_validation: false,
        max_block_gas: 100_000,
        max_txs_chain_count: 32,
        ..Default::default()
    };

    let limit = Limits {
        max_inputs: 3,
        min_outputs: 2,
        max_outputs: 3,
        utxo_id_range: 128,
        gas_limit_range: 10_000,
    };

    for _ in 0..ROUNDS_PER_TEST {
        stability_test(limit, config.clone());
    }
}

#[test]
fn stability_test__many_dependencies() {
    let config = Config {
        utxo_validation: false,
        max_block_gas: 10_000,
        ..Default::default()
    };

    let limit = Limits {
        max_inputs: 200,
        min_outputs: 1,
        max_outputs: 10,
        utxo_id_range: 255,
        gas_limit_range: 1_000,
    };

    for _ in 0..ROUNDS_PER_TEST {
        stability_test(limit, config.clone());
    }
}

#[test]
fn stability_test__many_conflicting_transactions_with_different_priority() {
    let config = Config {
        utxo_validation: false,
        max_block_gas: 10_000,
        pool_limits: PoolLimits {
            max_txs: 32,
            max_gas: 80_000,
            max_bytes_size: 1_000_000,
        },
        ..Default::default()
    };

    let limit = Limits {
        max_inputs: 200,
        min_outputs: 1,
        max_outputs: 10,
        utxo_id_range: 255,
        gas_limit_range: 10_000,
    };

    for _ in 0..ROUNDS_PER_TEST {
        stability_test(limit, config.clone());
    }
}

#[test]
fn stability_test__long_chain_of_transactions() {
    let config = Config {
        utxo_validation: false,
        max_block_gas: 10_000,
        max_txs_chain_count: 128,
        pool_limits: PoolLimits {
            max_txs: 1_000,
            max_gas: 80_000,
            max_bytes_size: 1_000_000_000,
        },
        ..Default::default()
    };

    let limit = Limits {
        max_inputs: 2,
        min_outputs: 1,
        max_outputs: 2,
        utxo_id_range: 255,
        gas_limit_range: 100,
    };

    for _ in 0..ROUNDS_PER_TEST {
        stability_test(limit, config.clone());
    }
}

#[test]
fn stability_test__long_chain_of_transactions_with_conflicts() {
    let config = Config {
        utxo_validation: false,
        max_block_gas: 10_000,
        max_txs_chain_count: 32,
        pool_limits: PoolLimits {
            max_txs: 1_000,
            max_gas: 80_000,
            max_bytes_size: 1_000_000_000,
        },
        ..Default::default()
    };

    let limit = Limits {
        max_inputs: 2,
        min_outputs: 1,
        max_outputs: 2,
        utxo_id_range: 255,
        gas_limit_range: 10_000,
    };

    for _ in 0..ROUNDS_PER_TEST {
        stability_test(limit, config.clone());
    }
}

#[test]
fn stability_test__wide_chain_of_transactions_with_conflicts() {
    let config = Config {
        utxo_validation: false,
        max_block_gas: 10_000,
        max_txs_chain_count: 32,
        pool_limits: PoolLimits {
            max_txs: 1_000,
            max_gas: 80_000,
            max_bytes_size: 1_000_000_000,
        },
        ..Default::default()
    };

    let limit = Limits {
        max_inputs: 5,
        min_outputs: 1,
        max_outputs: 5,
        utxo_id_range: 255,
        gas_limit_range: 10_000,
    };

    for _ in 0..ROUNDS_PER_TEST {
        stability_test(limit, config.clone());
    }
}
