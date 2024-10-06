use crate::{
    error::Error,
    ports::{
        AtomicView,
        GasPriceProvider,
        TxPoolPersistentStorage,
        WasmChecker,
        WasmValidityError,
    },
    GasPrice,
};
use fuel_core_storage::{
    Mappable,
    PredicateStorageRequirements,
    Result as StorageResult,
    StorageInspect,
    StorageRead,
    StorageSize,
};
use fuel_core_types::{
    entities::{
        coins::coin::{
            Coin,
            CompressedCoin,
        },
        relayer::message::Message,
    },
    fuel_tx::{
        BlobId,
        Bytes32,
        Contract,
        ContractId,
        UtxoId,
    },
    fuel_types::Nonce,
    fuel_vm::{
        BlobBytes,
        BlobData,
    },
};
use std::{
    borrow::Cow,
    collections::{
        HashMap,
        HashSet,
    },
    sync::{
        Arc,
        Mutex,
    },
};

#[derive(Default)]
pub struct Data {
    pub coins: HashMap<UtxoId, CompressedCoin>,
    pub contracts: HashMap<ContractId, Contract>,
    pub blobs: HashMap<BlobId, BlobBytes>,
    pub messages: HashMap<Nonce, Message>,
    pub spent_messages: HashSet<Nonce>,
}

#[derive(Clone, Default)]
pub struct MockDb {
    pub data: Arc<Mutex<Data>>,
}

impl MockDb {
    pub fn insert_coin(&self, coin: Coin) {
        self.data
            .lock()
            .unwrap()
            .coins
            .insert(coin.utxo_id, coin.compress());
    }

    pub fn insert_dummy_blob(&self, blob_id: BlobId) {
        self.data
            .lock()
            .unwrap()
            .blobs
            .insert(blob_id, vec![123; 123].into());
    }

    pub fn insert_message(&self, message: Message) {
        self.data
            .lock()
            .unwrap()
            .messages
            .insert(*message.id(), message);
    }

    pub fn spend_message(&self, id: Nonce) {
        self.data.lock().unwrap().spent_messages.insert(id);
    }
}

impl TxPoolPersistentStorage for MockDb {
    fn utxo(&self, utxo_id: &UtxoId) -> StorageResult<Option<CompressedCoin>> {
        Ok(self.data.lock().unwrap().coins.get(utxo_id).cloned())
    }

    fn contract_exist(&self, contract_id: &ContractId) -> StorageResult<bool> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .contracts
            .contains_key(contract_id))
    }

    fn blob_exist(&self, blob_id: &BlobId) -> StorageResult<bool> {
        Ok(self.data.lock().unwrap().blobs.contains_key(blob_id))
    }

    fn message(&self, id: &Nonce) -> StorageResult<Option<Message>> {
        Ok(self.data.lock().unwrap().messages.get(id).cloned())
    }
}

impl StorageRead<BlobData> for MockDb {
    fn read(
        &self,
        key: &<BlobData as Mappable>::Key,
        buf: &mut [u8],
    ) -> Result<Option<usize>, Self::Error> {
        let table = self.data.lock().unwrap();
        let bytes = table.blobs.get(key);

        let len = bytes.map(|bytes| {
            buf.copy_from_slice(bytes.0.as_slice());
            bytes.0.len()
        });
        Ok(len)
    }

    fn read_alloc(
        &self,
        key: &<BlobData as Mappable>::Key,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let table = self.data.lock().unwrap();
        let bytes = table.blobs.get(key);
        let bytes = bytes.map(|bytes| bytes.clone().0);
        Ok(bytes)
    }
}

impl StorageInspect<BlobData> for MockDb {
    type Error = ();

    fn get(
        &self,
        key: &<BlobData as Mappable>::Key,
    ) -> Result<Option<Cow<<BlobData as Mappable>::OwnedValue>>, Self::Error> {
        let table = self.data.lock().unwrap();
        let bytes = table.blobs.get(key);
        Ok(bytes.map(|b| Cow::Owned(b.clone())))
    }

    fn contains_key(
        &self,
        key: &<BlobData as Mappable>::Key,
    ) -> Result<bool, Self::Error> {
        Ok(self.data.lock().unwrap().blobs.contains_key(key))
    }
}

impl StorageSize<BlobData> for MockDb {
    fn size_of_value(
        &self,
        key: &<BlobData as Mappable>::Key,
    ) -> Result<Option<usize>, Self::Error> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .blobs
            .get(key)
            .map(|blob| blob.0.len()))
    }
}

impl PredicateStorageRequirements for MockDb {
    fn storage_error_to_string(error: Self::Error) -> String {
        format!("{:?}", error)
    }
}

#[derive(Clone)]
pub struct MockDBProvider(pub MockDb);

impl AtomicView for MockDBProvider {
    type LatestView = MockDb;

    fn latest_view(&self) -> StorageResult<Self::LatestView> {
        Ok(self.0.clone())
    }
}

#[derive(Debug, Clone)]
pub struct MockTxPoolGasPrice {
    pub gas_price: Option<GasPrice>,
}

impl MockTxPoolGasPrice {
    pub fn new(gas_price: GasPrice) -> Self {
        Self {
            gas_price: Some(gas_price),
        }
    }

    pub fn new_none() -> Self {
        Self { gas_price: None }
    }
}

#[async_trait::async_trait]
impl GasPriceProvider for MockTxPoolGasPrice {
    async fn next_gas_price(&self) -> Result<GasPrice, Error> {
        self.gas_price
            .ok_or(Error::GasPriceNotFound("Gas price not found".to_string()))
    }
}

pub struct MockWasmChecker {
    pub result: Result<(), WasmValidityError>,
}

impl MockWasmChecker {
    pub fn new(result: Result<(), WasmValidityError>) -> Self {
        Self { result }
    }
}

impl WasmChecker for MockWasmChecker {
    fn validate_uploaded_wasm(
        &self,
        _wasm_root: &Bytes32,
    ) -> Result<(), WasmValidityError> {
        self.result
    }
}