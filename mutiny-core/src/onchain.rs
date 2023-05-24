use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use bdk::chain::{BlockId, ConfirmationTime};
use bdk::psbt::PsbtUtils;
use bdk::template::DescriptorTemplateOut;
use bdk::{FeeRate, LocalUtxo, SignOptions, TransactionDetails, Wallet};
use bdk_esplora::{esplora_client, EsploraAsyncExt};
use bdk_macros::maybe_await;
use bip39::Mnemonic;
use bitcoin::psbt::PartiallySignedTransaction;
use bitcoin::util::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey};
use bitcoin::{Address, Network, OutPoint, Script, Transaction, Txid};
use esplora_client::AsyncClient;
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use lightning::log_debug;
use lightning::util::logger::Logger;

use crate::error::MutinyError;
use crate::fees::MutinyFeeEstimator;
use crate::labels::*;
use crate::logging::MutinyLogger;
use crate::storage::{MutinyStorage, OnChainStorage};

#[derive(Clone)]
pub struct OnChainWallet<S: MutinyStorage> {
    pub wallet: Arc<RwLock<Wallet<OnChainStorage<S>>>>,
    pub(crate) storage: S,
    pub network: Network,
    pub blockchain: Arc<AsyncClient>,
    pub fees: Arc<MutinyFeeEstimator<S>>,
    logger: Arc<MutinyLogger>,
}

impl<S: MutinyStorage> OnChainWallet<S> {
    pub fn new(
        mnemonic: &Mnemonic,
        db: S,
        network: Network,
        esplora: Arc<AsyncClient>,
        fees: Arc<MutinyFeeEstimator<S>>,
        logger: Arc<MutinyLogger>,
    ) -> Result<OnChainWallet<S>, MutinyError> {
        let seed = mnemonic.to_seed("");
        let xprivkey = ExtendedPrivKey::new_master(network, &seed)?;
        let account_number = 0;
        let (receive_descriptor_template, change_descriptor_template) =
            get_tr_descriptors_for_extended_key(xprivkey, network, account_number)?;

        let wallet = Wallet::new(
            receive_descriptor_template,
            Some(change_descriptor_template),
            OnChainStorage(db.clone()),
            network,
        )?;

        Ok(OnChainWallet {
            wallet: Arc::new(RwLock::new(wallet)),
            storage: db,
            network,
            blockchain: esplora,
            fees,
            logger,
        })
    }

    pub async fn sync(&self) -> Result<(), MutinyError> {
        // get first wallet lock that only needs to read
        let (checkpoints, spks) = {
            let wallet = self.wallet.try_read()?;
            let checkpoints = wallet.checkpoints();
            let spks = wallet
                .spks_of_all_keychains()
                .into_iter()
                .map(|(k, spks)| (k, spks))
                .collect();

            (checkpoints.clone(), spks)
        };

        let update = self
            .blockchain
            .scan(
                &checkpoints,
                spks,
                core::iter::empty(),
                core::iter::empty(),
                50,
                5,
            )
            .await?;
        // get new wallet lock for writing
        let mut wallet = self.wallet.try_write()?;
        wallet.apply_update(update)?;
        wallet.commit()?;

        Ok(())
    }

    pub(crate) async fn insert_tx(
        &self,
        tx: Transaction,
        position: ConfirmationTime,
        block_id: Option<BlockId>,
    ) -> Result<(), MutinyError> {
        match position {
            ConfirmationTime::Confirmed { .. } => {
                // if the transaction is confirmed and we have the block id,
                // we can insert it directly
                if let Some(block_id) = block_id {
                    let mut wallet = self.wallet.try_write()?;
                    wallet.insert_checkpoint(block_id)?;
                    wallet.insert_tx(tx, position)?;
                } else {
                    // if the transaction is confirmed and we don't have the block id,
                    // we should just sync the wallet otherwise we can get an error
                    // with the wallet being behind the blockchain
                    self.sync().await?
                }
            }
            ConfirmationTime::Unconfirmed => {
                // if the transaction is unconfirmed, we can just insert it
                let mut wallet = self.wallet.try_write()?;
                wallet.insert_tx(tx, position)?;
            }
        }

        Ok(())
    }

    pub fn list_utxos(&self) -> Result<Vec<LocalUtxo>, MutinyError> {
        Ok(self.wallet.try_read()?.list_unspent())
    }

    pub fn list_transactions(
        &self,
        include_raw: bool,
    ) -> Result<Vec<TransactionDetails>, MutinyError> {
        #[allow(deprecated)]
        Ok(self.wallet.try_read()?.list_transactions(include_raw))
    }

    pub fn get_transaction(
        &self,
        txid: Txid,
        include_raw: bool,
    ) -> Result<Option<TransactionDetails>, MutinyError> {
        Ok(self.wallet.try_read()?.get_tx(txid, include_raw))
    }

    #[allow(dead_code)]
    fn get_psbt_previous_labels(
        &self,
        psbt: &PartiallySignedTransaction,
    ) -> Result<Vec<String>, MutinyError> {
        // first get previous labels
        let address_labels = self.storage.get_address_labels()?;

        // get previous addresses
        let prev_addresses = psbt
            .inputs
            .iter()
            .filter_map(|i| {
                let address = if let Some(out) = i.witness_utxo.as_ref() {
                    Address::from_script(&out.script_pubkey, self.network).ok()
                } else {
                    None
                };

                address
            })
            .collect::<Vec<_>>();

        // get addresses from previous labels
        let prev_labels = prev_addresses
            .iter()
            .filter_map(|addr| address_labels.get(&addr.to_string()))
            .flatten()
            .cloned()
            .collect::<Vec<_>>();

        Ok(prev_labels)
    }

    #[allow(dead_code)]
    pub(crate) fn label_psbt(
        &self,
        psbt: &PartiallySignedTransaction,
        labels: Vec<String>,
    ) -> Result<(), MutinyError> {
        let mut prev_labels = vec![];

        // add on new labels
        prev_labels.extend(labels);

        // deduplicate labels and create aggregate label
        // we use a HashSet to deduplicate so we can retain the order of the labels
        let mut seen = HashSet::new();
        let agg_labels = prev_labels
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect::<Vec<_>>();

        // add output addresses to previous addresses
        let addresses = psbt
            .unsigned_tx
            .output
            .iter()
            .filter_map(|o| Address::from_script(&o.script_pubkey, self.network).ok())
            .collect::<Vec<_>>();

        // set label for send to address
        for addr in addresses {
            self.storage.set_address_labels(addr, agg_labels.clone())?;
        }

        Ok(())
    }

    pub fn create_signed_psbt(
        &self,
        send_to: Address,
        amount: u64,
        fee_rate: Option<f32>,
    ) -> Result<PartiallySignedTransaction, MutinyError> {
        if !send_to.is_valid_for_network(self.network) {
            return Err(MutinyError::IncorrectNetwork(send_to.network));
        }

        self.create_signed_psbt_to_spk(send_to.script_pubkey(), amount, fee_rate)
    }

    pub fn create_signed_psbt_to_spk(
        &self,
        spk: Script,
        amount: u64,
        fee_rate: Option<f32>,
    ) -> Result<PartiallySignedTransaction, MutinyError> {
        let mut wallet = self.wallet.try_write()?;

        let fee_rate = if let Some(rate) = fee_rate {
            FeeRate::from_sat_per_vb(rate)
        } else {
            let sat_per_kwu = self
                .fees
                .get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
            FeeRate::from_sat_per_kwu(sat_per_kwu as f32)
        };
        let (mut psbt, details) = {
            let mut builder = wallet.build_tx();
            builder
                .add_recipient(spk, amount)
                .enable_rbf()
                .fee_rate(fee_rate);
            builder.finish()?
        };
        log_debug!(self.logger, "Transaction details: {details:#?}");
        log_debug!(self.logger, "Unsigned PSBT: {psbt}");
        let finalized = wallet.sign(&mut psbt, SignOptions::default())?;
        log_debug!(self.logger, "finalized: {finalized}");
        Ok(psbt)
    }

    pub async fn send(
        &self,
        destination_address: Address,
        amount: u64,
        labels: Vec<String>,
        fee_rate: Option<f32>,
    ) -> Result<Txid, MutinyError> {
        let psbt = self.create_signed_psbt(destination_address, amount, fee_rate)?;
        self.label_psbt(&psbt, labels)?;

        let raw_transaction = psbt.extract_tx();
        let txid = raw_transaction.txid();

        maybe_await!(self.blockchain.broadcast(&raw_transaction))?;
        log_debug!(self.logger, "Transaction broadcast! TXID: {txid}");
        Ok(txid)
    }

    pub fn create_sweep_psbt(
        &self,
        destination_address: Address,
        fee_rate: Option<f32>,
    ) -> Result<PartiallySignedTransaction, MutinyError> {
        if !destination_address.is_valid_for_network(self.network) {
            return Err(MutinyError::IncorrectNetwork(destination_address.network));
        }

        let mut wallet = self.wallet.try_write()?;

        let fee_rate = if let Some(rate) = fee_rate {
            FeeRate::from_sat_per_vb(rate)
        } else {
            let sat_per_kwu = self
                .fees
                .get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
            FeeRate::from_sat_per_kwu(sat_per_kwu as f32)
        };
        let spk = destination_address.script_pubkey();
        let (mut psbt, details) = {
            let mut builder = wallet.build_tx();
            builder
                .drain_wallet() // Spend all outputs in this wallet.
                .drain_to(spk)
                .enable_rbf()
                .fee_rate(fee_rate);
            builder.finish()?
        };
        log_debug!(self.logger, "Transaction details: {details:#?}");
        log_debug!(self.logger, "Unsigned PSBT: {psbt}");
        let finalized = wallet.sign(&mut psbt, SignOptions::default())?;
        log_debug!(self.logger, "finalized: {finalized}");
        Ok(psbt)
    }

    pub async fn sweep(
        &self,
        destination_address: Address,
        labels: Vec<String>,
        fee_rate: Option<f32>,
    ) -> Result<Txid, MutinyError> {
        let psbt = self.create_sweep_psbt(destination_address, fee_rate)?;
        self.label_psbt(&psbt, labels)?;

        let raw_transaction = psbt.extract_tx();
        let txid = raw_transaction.txid();

        maybe_await!(self.blockchain.broadcast(&raw_transaction))?;
        log_debug!(self.logger, "Transaction broadcast! TXID: {txid}");
        Ok(txid)
    }

    /// Creates a PSBT that spends all the selected utxos a given output.
    /// A fee rate is not specified because it should be precalculated
    /// in the output's amount.
    pub(crate) fn create_sweep_psbt_to_output(
        &self,
        utxos: &[OutPoint],
        spk: Script,
        amount_sats: u64,
        sat_per_kwu: u32,
    ) -> Result<PartiallySignedTransaction, MutinyError> {
        let mut wallet = self.wallet.try_write()?;
        let (mut psbt, details) = {
            let mut builder = wallet.build_tx();
            builder
                .manually_selected_only()
                .add_utxos(utxos)?
                .fee_rate(FeeRate::from_sat_per_kwu(sat_per_kwu as f32))
                .add_recipient(spk, amount_sats);
            builder.finish()?
        };
        log_debug!(self.logger, "Transaction details: {details:#?}");
        log_debug!(self.logger, "Unsigned PSBT: {psbt}");
        let finalized = wallet.sign(&mut psbt, SignOptions::default())?;
        log_debug!(self.logger, "finalized: {finalized}");
        Ok(psbt)
    }

    pub fn estimate_tx_fee(
        &self,
        destination_address: Address,
        amount: u64,
        fee_rate: Option<f32>,
    ) -> Result<u64, MutinyError> {
        let psbt =
            self.create_signed_psbt_to_spk(destination_address.script_pubkey(), amount, fee_rate)?;

        psbt.fee_amount().ok_or(MutinyError::WalletOperationFailed)
    }
}

fn get_tr_descriptors_for_extended_key(
    master_xprv: ExtendedPrivKey,
    network: Network,
    account_number: u32,
) -> Result<(DescriptorTemplateOut, DescriptorTemplateOut), MutinyError> {
    let coin_type = match network {
        Network::Bitcoin => 0,
        Network::Testnet => 1,
        Network::Signet => 1,
        Network::Regtest => 1,
    };

    let base_path = DerivationPath::from_str("m/86'")?;
    let derivation_path = base_path.extend([
        ChildNumber::from_hardened_idx(coin_type)?,
        ChildNumber::from_hardened_idx(account_number)?,
    ]);

    let receive_descriptor_template = bdk::descriptor!(tr((
        master_xprv,
        derivation_path.extend([ChildNumber::Normal { index: 0 }])
    )))?;
    let change_descriptor_template = bdk::descriptor!(tr((
        master_xprv,
        derivation_path.extend([ChildNumber::Normal { index: 1 }])
    )))?;

    Ok((receive_descriptor_template, change_descriptor_template))
}

pub(crate) fn get_esplora_url(network: Network, user_provided_url: Option<String>) -> String {
    if let Some(url) = user_provided_url {
        url
    } else {
        match network {
            Network::Bitcoin => "https://mempool.space/api",
            Network::Testnet => "https://mempool.space/testnet/api",
            Network::Signet => "https://mutinynet.com/api",
            Network::Regtest => "http://localhost:3003",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use crate::test_utils::*;
    use bitcoin::Address;
    use esplora_client::Builder;
    use std::str::FromStr;
    use wasm_bindgen_test::{wasm_bindgen_test as test, wasm_bindgen_test_configure};
    wasm_bindgen_test_configure!(run_in_browser);

    async fn create_wallet() -> OnChainWallet<MemoryStorage> {
        let mnemonic = Mnemonic::from_str("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about").expect("could not generate");
        let esplora = Arc::new(
            Builder::new("https://blockstream.info/testnet/api/")
                .build_async()
                .unwrap(),
        );
        let db = MemoryStorage::new(Some(uuid::Uuid::new_v4().to_string()));
        let logger = Arc::new(MutinyLogger::default());
        let fees = Arc::new(MutinyFeeEstimator::new(
            db.clone(),
            esplora.clone(),
            logger.clone(),
        ));

        OnChainWallet::new(&mnemonic, db, Network::Testnet, esplora, fees, logger).unwrap()
    }

    #[test]
    async fn test_create_wallet() {
        let test_name = "create_wallet";
        log!("{}", test_name);
        let _wallet = create_wallet().await;
    }

    #[test]
    async fn test_label_psbt() {
        let test_name = "label_psbt";
        log!("{}", test_name);
        let wallet = create_wallet().await;

        let psbt = PartiallySignedTransaction::from_str("cHNidP8BAKACAAAAAqsJSaCMWvfEm4IS9Bfi8Vqz9cM9zxU4IagTn4d6W3vkAAAAAAD+////qwlJoIxa98SbghL0F+LxWrP1wz3PFTghqBOfh3pbe+QBAAAAAP7///8CYDvqCwAAAAAZdqkUdopAu9dAy+gdmI5x3ipNXHE5ax2IrI4kAAAAAAAAGXapFG9GILVT+glechue4O/p+gOcykWXiKwAAAAAAAEHakcwRAIgR1lmF5fAGwNrJZKJSGhiGDR9iYZLcZ4ff89X0eURZYcCIFMJ6r9Wqk2Ikf/REf3xM286KdqGbX+EhtdVRs7tr5MZASEDXNxh/HupccC1AaZGoqg7ECy0OIEhfKaC3Ibi1z+ogpIAAQEgAOH1BQAAAAAXqRQ1RebjO4MsRwUPJNPuuTycA5SLx4cBBBYAFIXRNTfy4mVAWjTbr6nj3aAfuCMIAAAA").unwrap();

        // set label for input
        let input_addr = Address::from_str("2Mx6uYKYGW5J6sV59e5NsdtCTsJYRxednbx").unwrap();
        let prev_label = "previous".to_string();
        wallet
            .storage
            .set_address_labels(input_addr, vec![prev_label])
            .unwrap();

        let send_to_addr = Address::from_str("mrKjeffvbnmKJURrLNdqLkfrptLrFtnkFx").unwrap();
        let change_addr = Address::from_str("mqfKJuj2Ea4RtXsKawQWrqosGeHFTrp6iZ").unwrap();
        let label = "test".to_string();

        let result = wallet.label_psbt(&psbt, vec![label.clone()]);
        assert!(result.is_ok());

        let expected_labels = vec![label.clone()];

        let addr_labels = wallet.storage.get_address_labels().unwrap();
        assert_eq!(addr_labels.len(), 3);
        assert_eq!(
            addr_labels.get(&send_to_addr.to_string()),
            Some(&expected_labels)
        );
        assert_eq!(
            addr_labels.get(&change_addr.to_string()),
            Some(&expected_labels)
        );

        let label = wallet.storage.get_label(&label).unwrap();
        assert!(label.is_some());
        assert_eq!(label.clone().unwrap().addresses.len(), 2);
        assert!(label.clone().unwrap().addresses.contains(&send_to_addr));
        assert!(label.unwrap().addresses.contains(&change_addr));
    }
}