use crate::encrypt::{decrypt, encrypt};
use crate::error::{MutinyError, MutinyStorageError};
use crate::ldkstorage::CHANNEL_MANAGER_KEY;
use crate::nodemanager::NodeStorage;
use anyhow::anyhow;
use bdk::chain::keychain::{KeychainChangeSet, KeychainTracker, PersistBackend};
use bdk::chain::sparse_chain::ChainPosition;
use bip39::Mnemonic;
use gloo_utils::format::JsValueSerdeExt;
use rexie::{ObjectStore, Rexie, TransactionMode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

pub(crate) const WALLET_DATABASE_NAME: &str = "wallet";
pub(crate) const WALLET_OBJECT_STORE_NAME: &str = "wallet_store";

const KEYCHAIN_STORE_KEY: &str = "keychain_store";
const MNEMONIC_KEY: &str = "mnemonic";
const NODES_KEY: &str = "nodes";
const FEE_ESTIMATES_KEY: &str = "fee_estimates";

#[derive(Clone)]
pub struct MutinyStorage {
    pub(crate) password: Option<String>,
    /// In-memory cache of the wallet data
    /// This is used to avoid having to read from IndexedDB on every get.
    /// This is a RwLock because we want to be able to read from it without blocking
    memory: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    pub(crate) indexed_db: Arc<Rexie>,
}

impl MutinyStorage {
    pub async fn new(password: String) -> Result<MutinyStorage, MutinyError> {
        let indexed_db = Arc::new(Self::build_indexed_db_database().await?);

        // If the password is empty, set to None
        let password = Some(password).filter(|pw| !pw.is_empty());

        let map = Self::read_all(&indexed_db, &password).await?;
        let memory = Arc::new(RwLock::new(map));

        Ok(MutinyStorage {
            password,
            memory,
            indexed_db,
        })
    }

    pub(crate) fn set<T>(&self, key: impl AsRef<str>, value: T) -> Result<(), MutinyError>
    where
        T: Serialize,
    {
        let key = key.as_ref().to_string();
        let data = serde_json::to_value(value)?;

        let indexed_db = self.indexed_db.clone();
        let password = self.password.clone();
        let key_clone = key.clone();
        let data_clone = data.clone();
        spawn_local(async move {
            Self::save_to_indexed_db(indexed_db, &password, &key_clone, &data_clone)
                .await
                .expect(&format!("Failed to save to indexed db: {key_clone}"))
        });

        let mut map = self
            .memory
            .try_write()
            .map_err(|e| MutinyError::write_err(e.into()))?;
        map.insert(key, data);

        Ok(())
    }

    async fn save_to_indexed_db(
        indexed_db: Arc<Rexie>,
        password: &Option<String>,
        key: &str,
        data: &serde_json::Value,
    ) -> Result<(), MutinyError> {
        let tx = indexed_db
            .as_ref()
            .transaction(&[WALLET_OBJECT_STORE_NAME], TransactionMode::ReadWrite)?;

        let store = tx.store(WALLET_OBJECT_STORE_NAME)?;

        // Only bother encrypting if a password is set
        let json = match password {
            Some(pw) if Self::needs_encryption(key) => {
                let str = serde_json::to_string(data)?;
                let ciphertext = encrypt(&str, pw);
                let json = serde_json::Value::String(ciphertext);
                JsValue::from_serde(&json)?
            }
            _ => JsValue::from_serde(&data)?,
        };

        // save to indexed db
        store.put(&json, Some(&JsValue::from(key))).await?;

        tx.done().await?;

        Ok(())
    }

    pub(crate) fn get<T>(&self, key: impl AsRef<str>) -> Result<Option<T>, MutinyError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let map = self
            .memory
            .try_read()
            .map_err(|e| MutinyError::read_err(e.into()))?;
        match map.get(key.as_ref()) {
            None => Ok(None),
            Some(value) => {
                let data: T = serde_json::from_value(value.to_owned())?;
                Ok(Some(data))
            }
        }
    }

    pub(crate) fn scan<T>(
        &self,
        prefix: &str,
        suffix: Option<&str>,
    ) -> Result<HashMap<String, T>, MutinyError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let map = self
            .memory
            .try_read()
            .map_err(|e| MutinyError::read_err(e.into()))?;

        Ok(map
            .keys()
            .filter(|key| {
                key.starts_with(prefix) && (suffix.is_none() || key.ends_with(suffix.unwrap()))
            })
            .filter_map(|key| {
                self.get(key)
                    .ok()
                    .flatten()
                    .map(|value: T| (key.to_owned(), value))
            })
            .collect())
    }

    pub(crate) async fn insert_mnemonic(
        &self,
        mnemonic: Mnemonic,
    ) -> Result<Mnemonic, MutinyError> {
        // Instead of calling self.set we manually write to indexed db
        // so we get a guarantee that the mnemonic is saved before we return

        let data = serde_json::to_value(mnemonic.to_string())?;
        Self::save_to_indexed_db(self.indexed_db.clone(), &self.password, MNEMONIC_KEY, &data)
            .await?;
        Ok(mnemonic)
    }

    pub(crate) async fn get_mnemonic(&self) -> Result<Mnemonic, MutinyError> {
        let tx = self
            .indexed_db
            .transaction(&[WALLET_OBJECT_STORE_NAME], TransactionMode::ReadOnly)?;
        let store = tx.store(WALLET_OBJECT_STORE_NAME)?;

        let key = JsValue::from(MNEMONIC_KEY);
        let json = store.get(&key).await?;
        let value: Option<String> = json.into_serde()?;

        let mnemonic = match value {
            Some(mnemonic) => Mnemonic::from_str(&mnemonic)?,
            None => return Err(MutinyError::InvalidMnemonic), // maybe need a better error
        };

        tx.done().await?;

        Ok(mnemonic)
    }

    pub(crate) fn get_nodes(&self) -> Result<NodeStorage, MutinyError> {
        let res: Option<NodeStorage> = self.get(NODES_KEY)?;
        match res {
            Some(nodes) => Ok(nodes),
            None => Ok(NodeStorage::default()),
        }
    }

    pub(crate) fn insert_nodes(&self, nodes: NodeStorage) -> Result<(), MutinyError> {
        self.set(NODES_KEY, nodes)
    }

    pub(crate) fn get_fee_estimates(&self) -> Result<Option<HashMap<String, f64>>, MutinyError> {
        self.get(FEE_ESTIMATES_KEY)
    }

    pub(crate) fn insert_fee_estimates(
        &self,
        fees: HashMap<String, f64>,
    ) -> Result<(), MutinyError> {
        self.set(FEE_ESTIMATES_KEY, fees)
    }

    #[cfg(test)]
    pub(crate) async fn reload_from_indexed_db(&self) -> Result<(), MutinyError> {
        let map = Self::read_all(&self.indexed_db, &self.password).await?;
        let mut memory = self
            .memory
            .try_write()
            .map_err(|e| MutinyError::write_err(e.into()))?;
        *memory = map;
        Ok(())
    }

    pub(crate) async fn has_mnemonic() -> Result<bool, MutinyError> {
        let indexed_db = Self::build_indexed_db_database().await?;
        let tx = indexed_db.transaction(&[WALLET_OBJECT_STORE_NAME], TransactionMode::ReadOnly)?;
        let store = tx.store(WALLET_OBJECT_STORE_NAME)?;

        let key = JsValue::from(MNEMONIC_KEY);
        let json = store.get(&key).await?;
        let value: Option<String> = json.into_serde()?;

        Ok(value.is_some())
    }

    async fn build_indexed_db_database() -> Result<Rexie, MutinyError> {
        let rexie = Rexie::builder(WALLET_DATABASE_NAME)
            .version(1)
            .add_object_store(ObjectStore::new(WALLET_OBJECT_STORE_NAME))
            .build()
            .await?;

        Ok(rexie)
    }

    pub(crate) async fn read_all(
        indexed_db: &Rexie,
        password: &Option<String>,
    ) -> Result<HashMap<String, serde_json::Value>, MutinyError> {
        let tx = indexed_db.transaction(&[WALLET_OBJECT_STORE_NAME], TransactionMode::ReadOnly)?;

        let store = tx.store(WALLET_OBJECT_STORE_NAME)?;

        let mut map = HashMap::new();
        let all_json = store.get_all(None, None, None, None).await?;
        for (key, value) in all_json {
            let key = key
                .as_string()
                .ok_or(MutinyError::read_err(MutinyStorageError::Other(anyhow!(
                    "key from indexedDB is not a string"
                ))))?;
            let json: Option<serde_json::Value> = match password {
                Some(pw) if Self::needs_encryption(&key) => {
                    let str: String = value.into_serde()?;
                    let ciphertext = decrypt(&str, pw);
                    serde_json::from_str(&ciphertext)?
                }
                _ => value.into_serde()?,
            };

            if let Some(json) = json {
                map.insert(key, json);
            }
        }

        Ok(map)
    }

    fn needs_encryption(key: &str) -> bool {
        match key {
            MNEMONIC_KEY => true,
            str if str.starts_with(CHANNEL_MANAGER_KEY) => true,
            _ => false,
        }
    }

    pub(crate) async fn import(json: serde_json::Value) -> Result<(), MutinyError> {
        Self::clear().await?;
        let indexed_db = Self::build_indexed_db_database().await?;
        let tx = indexed_db.transaction(&[WALLET_OBJECT_STORE_NAME], TransactionMode::ReadWrite)?;
        let store = tx.store(WALLET_OBJECT_STORE_NAME)?;

        let map = json
            .as_object()
            .ok_or(MutinyError::write_err(MutinyStorageError::Other(anyhow!(
                "json is not an object"
            ))))?;

        for (key, value) in map {
            let key = JsValue::from(key);
            let value = JsValue::from_serde(&value)?;
            store.put(&value, Some(&key)).await?;
        }

        tx.done().await?;

        Ok(())
    }

    pub(crate) async fn clear() -> Result<(), MutinyError> {
        let indexed_db = Self::build_indexed_db_database().await?;
        let tx = indexed_db.transaction(&[WALLET_OBJECT_STORE_NAME], TransactionMode::ReadWrite)?;
        let store = tx.store(WALLET_OBJECT_STORE_NAME)?;

        store.clear().await?;

        tx.done().await?;

        Ok(())
    }
}

impl<K, P> PersistBackend<K, P> for MutinyStorage
where
    K: Ord + Clone + core::fmt::Debug,
    P: ChainPosition,
    KeychainChangeSet<K, P>: serde::Serialize + serde::de::DeserializeOwned,
{
    type WriteError = MutinyError;
    type LoadError = MutinyError;

    fn append_changeset(
        &mut self,
        changeset: &KeychainChangeSet<K, P>,
    ) -> Result<(), Self::WriteError> {
        if changeset.is_empty() {
            return Ok(());
        }

        match self.get::<KeychainChangeSet<K, P>>(KEYCHAIN_STORE_KEY)? {
            Some(mut keychain_store) => {
                keychain_store.append(changeset.clone());
                self.set(KEYCHAIN_STORE_KEY, keychain_store)
            }
            None => self.set(KEYCHAIN_STORE_KEY, changeset),
        }
    }

    fn load_into_keychain_tracker(
        &mut self,
        tracker: &mut KeychainTracker<K, P>,
    ) -> Result<(), Self::LoadError> {
        if let Some(k) = self.get(KEYCHAIN_STORE_KEY)? {
            tracker.apply_changeset(k);
        }

        Ok(())
    }
}