#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::cast_possible_truncation)]
#![deny(unused_crate_dependencies)]
#![deny(warnings)]

pub mod compress;
pub mod decompress;
mod eviction_policy;
pub mod ports;
mod tables;

pub use tables::{
    RegistryKeyspace,
    RegistryKeyspaceValue,
};

use serde::{
    Deserialize,
    Serialize,
};

use fuel_core_types::{
    blockchain::header::PartialBlockHeader,
    fuel_tx::CompressedTransaction,
    fuel_types::Bytes32,
};
use tables::RegistrationsPerTable;

/// Compressed block, without the preceding version byte.
#[derive(Clone, Serialize, Deserialize)]
struct CompressedBlockPayloadV0 {
    /// Temporal registry insertions
    registrations: RegistrationsPerTable,
    /// Merkle root of the temporal registry state
    registrations_root: Bytes32,
    /// Compressed block header
    header: PartialBlockHeader,
    /// Compressed transactions
    transactions: Vec<CompressedTransaction>,
}

/// Versioned compressed block.
#[derive(Clone, Serialize, Deserialize)]
enum CompressedBlock {
    V0(CompressedBlockPayloadV0),
}

#[cfg(test)]
mod tests {
    use fuel_core_types::{
        blockchain::{
            header::{
                ApplicationHeader,
                ConsensusHeader,
            },
            primitives::Empty,
        },
        fuel_compression::RegistryKey,
        fuel_tx::{
            input::PredicateCode,
            Address,
            AssetId,
            ContractId,
            ScriptCode,
        },
        tai64::Tai64,
    };
    use proptest::prelude::*;
    use tables::PerRegistryKeyspaceMap;

    use super::*;

    fn keyspace() -> impl Strategy<Value = RegistryKeyspace> {
        prop_oneof![
            Just(RegistryKeyspace::Address),
            Just(RegistryKeyspace::AssetId),
            Just(RegistryKeyspace::ContractId),
            Just(RegistryKeyspace::ScriptCode),
            Just(RegistryKeyspace::PredicateCode),
        ]
    }

    fn keyspace_value() -> impl Strategy<Value = RegistryKeyspaceValue> {
        (keyspace(), prop::array::uniform32(0..u8::MAX)).prop_map(|(keyspace, value)| {
            match keyspace {
                RegistryKeyspace::Address => {
                    RegistryKeyspaceValue::Address(Address::new(value))
                }
                RegistryKeyspace::AssetId => {
                    RegistryKeyspaceValue::AssetId(AssetId::new(value))
                }
                RegistryKeyspace::ContractId => {
                    RegistryKeyspaceValue::ContractId(ContractId::new(value))
                }
                RegistryKeyspace::ScriptCode => {
                    let len = (value[0] % 32) as usize;
                    RegistryKeyspaceValue::ScriptCode(ScriptCode {
                        bytes: value[..len].to_vec(),
                    })
                }
                RegistryKeyspace::PredicateCode => {
                    let len = (value[0] % 32) as usize;
                    RegistryKeyspaceValue::PredicateCode(PredicateCode {
                        bytes: value[..len].to_vec(),
                    })
                }
            }
        })
    }

    proptest! {
        /// Serialization for compressed transactions is already tested in fuel-vm,
        /// but the rest of the block de/serialization is be tested here.
        #[test]
        fn postcard_roundtrip(
            da_height in 0..u64::MAX,
            prev_root in prop::array::uniform32(0..u8::MAX),
            height in 0..u32::MAX,
            consensus_parameters_version in 0..u32::MAX,
            state_transition_bytecode_version in 0..u32::MAX,
            registrations_root in prop::array::uniform32(0..u8::MAX),
            registration_inputs in prop::collection::vec(
                (keyspace_value(), prop::num::u16::ANY).prop_map(|(v, rk)| {
                    let k = RegistryKey::try_from(rk as u32).unwrap();
                    (k, v)
                }),
                0..123
            ),
        ) {
            let mut registrations: PerRegistryKeyspaceMap = Default::default();

            for (key, ksv) in registration_inputs {
                registrations.insert(key, ksv);
            }

            let header = PartialBlockHeader {
                application: ApplicationHeader {
                    da_height: da_height.into(),
                    consensus_parameters_version,
                    state_transition_bytecode_version,
                    generated: Empty,
                },
                consensus: ConsensusHeader {
                    prev_root: prev_root.into(),
                    height: height.into(),
                    time: Tai64::UNIX_EPOCH,
                    generated: Empty
                }
            };
            let original = CompressedBlockPayloadV0 {
                registrations: RegistrationsPerTable::try_from(registrations).unwrap(),
                registrations_root: registrations_root.into(),
                header,
                transactions: vec![],
            };

            let compressed = postcard::to_allocvec(&original).unwrap();
            let decompressed: CompressedBlockPayloadV0 =
                postcard::from_bytes(&compressed).unwrap();

            let CompressedBlockPayloadV0 {
                registrations,
                registrations_root,
                header,
                transactions,
            } = decompressed;

            assert_eq!(registrations, original.registrations);
            assert_eq!(registrations_root, original.registrations_root);

            assert_eq!(header.da_height, da_height.into());
            assert_eq!(*header.prev_root(), prev_root.into());
            assert_eq!(*header.height(), height.into());
            assert_eq!(header.consensus_parameters_version, consensus_parameters_version);
            assert_eq!(header.state_transition_bytecode_version, state_transition_bytecode_version);

            assert!(transactions.is_empty());
        }
    }
}
