use crate::fee_rate_estimator::EstimateFeeRate;
use crate::node::Fee;
use crate::node::Storage;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use bdk::blockchain::Blockchain;
use bdk::blockchain::GetBlockHash;
use bdk::blockchain::GetHeight;
use bdk::database::BatchDatabase;
use bdk::psbt::PsbtUtils;
use bdk::wallet::AddressIndex;
use bdk::FeeRate;
use bdk::SignOptions;
use bdk::SyncOptions;
use bdk::TransactionDetails;
use bdk_coin_select::metrics::LowestFee;
use bdk_coin_select::Candidate;
use bdk_coin_select::ChangePolicy;
use bdk_coin_select::CoinSelector;
use bdk_coin_select::DrainWeights;
use bdk_coin_select::Target;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::psbt::PartiallySignedTransaction;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::BlockHash;
use bitcoin::OutPoint;
use bitcoin::Script;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::Txid;
use bitcoin::VarInt;
use dlc_manager::Utxo;
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::chaininterface::ConfirmationTarget;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Taken from mempool.space
const AVG_SEGWIT_TX_WEIGHT_VB: usize = 140;

pub struct Wallet<D, B, F, N>
where
    D: BatchDatabase,
    B: Blockchain,
    F: EstimateFeeRate,
{
    // A BDK blockchain used for wallet sync.
    pub(crate) blockchain: Arc<B>,
    // A BDK on-chain wallet.
    inner: Mutex<bdk::Wallet<D>>,
    settings: RwLock<WalletSettings>,
    fee_rate_estimator: Arc<F>,
    // Only cleared upon restart. This means that if a locked outpoint ends up unspent, it will
    // remain locked until the binary is restarted.
    locked_outpoints: Mutex<Vec<OutPoint>>,
    node_storage: Arc<N>,
}

#[derive(Clone, Debug)]
pub struct WalletSettings {
    pub max_allowed_tx_fee_rate_when_opening_channel: Option<u32>,
    pub jit_channels_enabled: bool,
}

impl Default for WalletSettings {
    fn default() -> Self {
        Self {
            max_allowed_tx_fee_rate_when_opening_channel: None,
            jit_channels_enabled: true,
        }
    }
}

impl<D, B, F, N> Wallet<D, B, F, N>
where
    D: BatchDatabase,
    B: Blockchain,
    F: EstimateFeeRate,
    N: Storage,
{
    pub(crate) fn new(
        blockchain: B,
        wallet: bdk::Wallet<D>,
        fee_rate_estimator: Arc<F>,
        node_storage: Arc<N>,
        settings: WalletSettings,
    ) -> Self {
        let inner = Mutex::new(wallet);
        let settings = RwLock::new(settings);

        Self {
            blockchain: Arc::new(blockchain),
            inner,
            settings,
            fee_rate_estimator,
            locked_outpoints: Mutex::new(vec![]),
            node_storage,
        }
    }

    pub fn bdk_lock(&self) -> MutexGuard<bdk::Wallet<D>> {
        self.inner.lock()
    }

    pub async fn update_settings(&self, settings: WalletSettings) {
        tracing::info!(?settings, "Updating wallet settings");
        *self.settings.write().await = settings;
    }

    pub async fn settings(&self) -> WalletSettings {
        self.settings.read().await.clone()
    }

    /// Update the internal BDK wallet database with the blockchain.
    pub fn sync(&self) -> Result<()> {
        let wallet_lock = self.bdk_lock();

        let now = Instant::now();

        tracing::info!("Started on-chain sync");

        wallet_lock.sync(&self.blockchain, SyncOptions::default())?;

        let height = self.blockchain.get_height()?;

        tracing::info!(
            duration = now.elapsed().as_millis(),
            latest_height = height,
            "Finished on-chain sync",
        );

        self.locked_outpoints.lock().clear();

        Ok(())
    }

    pub fn get_fee_rate(&self, confirmation_target: ConfirmationTarget) -> FeeRate {
        self.fee_rate_estimator.estimate(confirmation_target)
    }

    pub(crate) async fn create_funding_transaction(
        &self,
        output_script: Script,
        value_sats: u64,
        fee_rate: FeeRate,
    ) -> Result<Transaction> {
        let mut locked_utxos = self.locked_outpoints.lock();
        let psbt = self.build_psbt(
            output_script,
            value_sats,
            Fee::FeeRate(fee_rate),
            locked_utxos.clone(),
        )?;

        let transaction = psbt.extract_tx();

        let prev_outpoints = transaction
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect::<Vec<_>>();

        locked_utxos.extend(prev_outpoints);

        Ok(transaction)
    }

    pub(crate) fn get_last_unused_address(&self) -> Result<Address> {
        Ok(self
            .bdk_lock()
            .get_address(AddressIndex::LastUnused)?
            .address)
    }

    pub(crate) fn get_new_address(&self) -> Result<Address> {
        Ok(self.bdk_lock().get_address(AddressIndex::New)?.address)
    }

    pub fn is_mine(&self, script: &Script) -> Result<bool> {
        Ok(self.bdk_lock().is_mine(script)?)
    }

    pub(crate) fn get_balance(&self) -> Result<bdk::Balance> {
        Ok(self.bdk_lock().get_balance()?)
    }

    pub fn get_utxos(&self) -> Result<Vec<bdk::LocalUtxo>> {
        let utxos = self.bdk_lock().list_unspent()?;
        Ok(utxos)
    }

    pub fn get_utxos_for_dlc_funding_transaction(
        &self,
        amount: u64,
        fee_rate: Option<u64>,
        should_lock_utxos: bool,
    ) -> Result<Vec<Utxo>> {
        let network = {
            let bdk = self.bdk_lock();
            bdk.network()
        };

        let fee_rate = fee_rate.map(|fee_rate| fee_rate as f32).unwrap_or_else(|| {
            self.get_fee_rate(ConfirmationTarget::Normal)
                .as_sat_per_vb()
        });

        // Get temporarily reserved UTXOs from in-memory storage.
        let mut reserved_outpoints = self.locked_outpoints.lock();

        let utxos = self.get_utxos()?;

        // Filter out reserved and spent UTXOs to prevent double-spending attempts.
        let utxos = utxos
            .iter()
            .filter(|utxo| !reserved_outpoints.contains(&utxo.outpoint))
            .filter(|utxo| !utxo.is_spent)
            .collect::<Vec<_>>();

        let candidates = utxos
            .iter()
            .map(|utxo| {
                let tx_in = TxIn {
                    previous_output: utxo.outpoint,
                    ..Default::default()
                };

                // Inspired by `rust-bitcoin:0.30.2`.
                let segwit_weight = {
                    let legacy_weight = {
                        let script_sig_size = tx_in.script_sig.len();
                        (36 + VarInt(script_sig_size as u64).len() + script_sig_size + 4) * 4
                    };

                    legacy_weight + tx_in.witness.serialized_len()
                };

                // The 10101 wallet always generates SegWit addresses.
                //
                // TODO: Rework this once we use Taproot.
                let is_witness_program = true;

                Candidate::new(utxo.txout.value, segwit_weight as u32, is_witness_program)
            })
            .collect::<Vec<_>>();

        // This is a standard base weight (without inputs or change outputs) for on-chain DLCs. We
        // assume that this value is still correct for DLC channels.
        let funding_tx_base_weight = 212;

        let target = Target {
            feerate: bdk_coin_select::FeeRate::from_sat_per_vb(fee_rate),
            min_fee: 0,
            value: amount,
        };

        let mut coin_selector = CoinSelector::new(&candidates, funding_tx_base_weight);

        let dust_limit = 0;
        let long_term_feerate = bdk_coin_select::FeeRate::default_min_relay_fee();

        let change_policy = ChangePolicy::min_value_and_waste(
            DrainWeights::default(),
            dust_limit,
            target.feerate,
            long_term_feerate,
        );

        let metric = LowestFee {
            target,
            long_term_feerate,
            change_policy,
        };

        coin_selector
            .run_bnb(metric, 100_000)
            .context("Failed to select coins")?;

        debug_assert!(coin_selector.is_target_met(target));

        let indices = coin_selector.selected_indices();

        let mut selected_utxos: Vec<Utxo> = Vec::with_capacity(indices.len());
        for index in indices {
            let utxo = &utxos[*index];

            let address = Address::from_script(&utxo.txout.script_pubkey, network)
                .expect("to be a valid address");

            let utxo = Utxo {
                tx_out: utxo.txout.clone(),
                outpoint: utxo.outpoint,
                address,
                redeem_script: Script::new(),
                reserved: false,
            };

            if should_lock_utxos {
                // Add selected UTXOs to reserve to prevent future double-spend attempts.
                reserved_outpoints.push(utxo.outpoint);
            }

            selected_utxos.push(utxo);
        }

        Ok(selected_utxos)
    }

    /// Build the PSBT for sending funds to a given script and signs it
    fn build_psbt(
        &self,
        recipient: Script,
        amount_sat_or_drain: u64,
        fee: Fee,
        locked_utxos: Vec<OutPoint>,
    ) -> Result<PartiallySignedTransaction> {
        let locked_wallet = self.bdk_lock();
        let mut tx_builder = locked_wallet.build_tx();

        for outpoint in locked_utxos.iter() {
            tx_builder.add_unspendable(*outpoint);
        }

        if amount_sat_or_drain > 0 {
            tx_builder
                .add_recipient(recipient, amount_sat_or_drain)
                .enable_rbf();
        } else {
            tx_builder.drain_wallet().drain_to(recipient).enable_rbf();
        }

        match fee {
            Fee::Priority(target) => tx_builder.fee_rate(self.fee_rate_estimator.estimate(target)),
            Fee::FeeRate(fee_rate) => tx_builder.fee_rate(fee_rate),
        };

        let mut psbt = match tx_builder.finish() {
            Ok((psbt, _)) => {
                tracing::trace!("Created PSBT: {:?}", psbt);
                psbt
            }
            Err(err) => {
                bail!(err)
            }
        };

        match locked_wallet.sign(&mut psbt, SignOptions::default()) {
            Ok(finalized) => {
                if !finalized {
                    bail!("On chain creation failed");
                }
            }
            Err(err) => {
                bail!(err)
            }
        }

        Ok(psbt)
    }

    /// Estimate the fee for sending funds to a given address
    pub(crate) fn calculate_fee(
        &self,
        address: &Address,
        amount_sat_or_drain: u64,
        confirmation_target: ConfirmationTarget,
    ) -> Result<Amount> {
        let locked_utxos = self.locked_outpoints.lock();
        let psbt = self.build_psbt(
            address.script_pubkey(),
            amount_sat_or_drain,
            Fee::Priority(confirmation_target),
            locked_utxos.clone(),
        );

        let fee_sat = match psbt {
            Ok(psbt) => psbt
                .fee_amount()
                .context("Fee info could not be calculated")?,
            Err(_) => {
                let rate = self.fee_rate_estimator.estimate(confirmation_target);
                rate.fee_vb(AVG_SEGWIT_TX_WEIGHT_VB)
            }
        };

        Ok(Amount::from_sat(fee_sat))
    }

    /// Send funds to the given address.
    ///
    /// If `amount_sat_or_drain` is `0` the wallet will be drained, i.e., all available funds
    /// will be spent.
    pub(crate) fn send_to_address(
        &self,
        address: &Address,
        amount_sat_or_drain: u64,
        fee: Fee,
    ) -> Result<Txid> {
        let mut locked_utxos = self.locked_outpoints.lock();
        let tx = self
            .build_psbt(
                address.script_pubkey(),
                amount_sat_or_drain,
                fee,
                locked_utxos.clone(),
            )?
            .extract_tx();

        let prev_outpoints = tx
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect::<Vec<_>>();

        locked_utxos.extend(prev_outpoints);

        let txid = self.broadcast_transaction(&tx)?;

        if amount_sat_or_drain > 0 {
            tracing::info!(
                "Created new transaction {} sending {}sats on-chain to address {}",
                txid,
                amount_sat_or_drain,
                address
            );
        } else {
            tracing::info!(
                "Created new transaction {} sending all available on-chain funds to address {}",
                txid,
                address
            );
        }

        Ok(txid)
    }

    pub fn tip(&self) -> Result<(u32, BlockHash)> {
        let height = self.blockchain.get_height()?;
        let hash = self.blockchain.get_block_hash(height as u64)?;

        Ok((height, hash))
    }

    pub fn on_chain_transaction_list(&self) -> Result<Vec<TransactionDetails>> {
        let wallet_lock = self.bdk_lock();
        wallet_lock
            .list_transactions(false)
            .context("Failed to list on chain transactions")
    }

    pub fn get_transaction(&self, txid: &Txid) -> Result<Option<TransactionDetails>> {
        let wallet_lock = self.bdk_lock();
        let transaction_details = wallet_lock.get_tx(txid, false)?;
        Ok(transaction_details)
    }

    pub fn broadcast_transaction(&self, tx: &Transaction) -> Result<Txid> {
        let txid = tx.txid();

        tracing::info!(%txid, raw_tx = %serialize_hex(&tx), "Broadcasting transaction");

        if let Err(e) = self.node_storage.upsert_transaction(tx.into()) {
            tracing::error!("Failed to store transaction {txid}. Error: {e:#}");
        }

        self.blockchain
            .broadcast(tx)
            .map_err(|e| anyhow!("Failed to broadcast transaction {txid}. {e:#}"))?;

        Ok(txid)
    }
}

impl<D, B, F, N> BroadcasterInterface for Wallet<D, B, F, N>
where
    D: BatchDatabase,
    B: Blockchain,
    F: EstimateFeeRate,
    N: Storage,
{
    fn broadcast_transactions(&self, txs: &[&Transaction]) {
        for tx in txs {
            if let Err(e) = self.broadcast_transaction(tx) {
                tracing::error!(
                    txid = %tx.txid(),
                    "Error when broadcasting transaction: {e:#}"
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
struct UtxoWrap {
    utxo: Utxo,
}

impl rust_bitcoin_coin_selection::Utxo for UtxoWrap {
    fn get_value(&self) -> u64 {
        self.utxo.tx_out.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;
    use crate::fee_rate_estimator::EstimateFeeRate;
    use crate::ldk_node_wallet::Wallet;
    use anyhow::Result;
    use bdk::blockchain::Blockchain;
    use bdk::blockchain::Capability;
    use bdk::blockchain::GetBlockHash;
    use bdk::blockchain::GetHeight;
    use bdk::blockchain::GetTx;
    use bdk::blockchain::Progress;
    use bdk::blockchain::WalletSync;
    use bdk::database::BatchDatabase;
    use bdk::populate_test_db;
    use bdk::testutils;
    use bdk::BlockTime;
    use bdk::Error;
    use bdk::FeeRate;
    use bitcoin::secp256k1::PublicKey;
    use bitcoin::util::bip32::ExtendedPrivKey;
    use bitcoin::Amount;
    use bitcoin::BlockHash;
    use bitcoin::Network;
    use bitcoin::Script;
    use bitcoin::Transaction;
    use bitcoin::Txid;
    use lightning::chain::chaininterface::ConfirmationTarget;
    use rand::thread_rng;
    use rand::CryptoRng;
    use rand::RngCore;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::sync::Arc;

    #[tokio::test]
    async fn wallet_with_two_utxo_should_be_able_to_fund_twice_but_not_three_times() {
        let mut rng = thread_rng();
        let test_wallet = new_test_wallet(&mut rng, Amount::from_btc(1.0).unwrap(), 2).unwrap();
        let wallet = Wallet::new(
            DummyEsplora,
            test_wallet,
            Arc::new(DummyFeeRateEstimator),
            Arc::new(DummyNodeStorage),
            WalletSettings::default(),
        );

        let fee_rate = FeeRate::from_sat_per_vb(10.0);
        let _ = wallet
            .create_funding_transaction(
                Script::new(),
                Amount::from_btc(0.5).unwrap().to_sat(),
                fee_rate,
            )
            .await
            .unwrap();
        let _ = wallet
            .create_funding_transaction(
                Script::new(),
                Amount::from_btc(0.5).unwrap().to_sat(),
                fee_rate,
            )
            .await
            .unwrap();
        assert!(wallet
            .create_funding_transaction(
                Script::new(),
                Amount::from_btc(0.5).unwrap().to_sat(),
                fee_rate,
            )
            .await
            .is_err());
    }

    fn new_test_wallet(
        rng: &mut (impl RngCore + CryptoRng),
        utxo_amount: Amount,
        num_utxos: u8,
    ) -> Result<bdk::Wallet<bdk::database::MemoryDatabase>> {
        new_test_wallet_from_database(
            rng,
            utxo_amount,
            num_utxos,
            bdk::database::MemoryDatabase::new(),
        )
    }

    fn new_test_wallet_from_database<DB: BatchDatabase>(
        rng: &mut (impl RngCore + CryptoRng),
        utxo_amount: Amount,
        num_utxos: u8,
        mut database: DB,
    ) -> Result<bdk::Wallet<DB>> {
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);

        let key = ExtendedPrivKey::new_master(Network::Regtest, &seed)?;
        let descriptors = testutils!(@descriptors (&format!("wpkh({key}/*)")));

        for index in 0..num_utxos {
            populate_test_db!(
                &mut database,
                testutils! {
                    @tx ( (@external descriptors, index as u32) => utxo_amount.to_sat() ) (@confirmations 1)
                },
                Some(100)
            );
        }

        let wallet = bdk::Wallet::new(&descriptors.0, None, Network::Regtest, database)?;

        Ok(wallet)
    }

    struct DummyFeeRateEstimator;

    impl EstimateFeeRate for DummyFeeRateEstimator {
        fn estimate(&self, _: ConfirmationTarget) -> FeeRate {
            FeeRate::from_sat_per_vb(1.0)
        }
    }

    struct DummyEsplora;

    impl WalletSync for DummyEsplora {
        fn wallet_setup<D: BatchDatabase>(
            &self,
            _: &RefCell<D>,
            _: Box<dyn Progress>,
        ) -> std::result::Result<(), Error> {
            unimplemented!()
        }
    }

    impl GetHeight for DummyEsplora {
        fn get_height(&self) -> std::result::Result<u32, Error> {
            unimplemented!()
        }
    }

    impl GetTx for DummyEsplora {
        fn get_tx(&self, _: &Txid) -> std::result::Result<Option<Transaction>, Error> {
            unimplemented!()
        }
    }

    impl GetBlockHash for DummyEsplora {
        fn get_block_hash(&self, _: u64) -> std::result::Result<BlockHash, Error> {
            unimplemented!()
        }
    }

    impl Blockchain for DummyEsplora {
        fn get_capabilities(&self) -> HashSet<Capability> {
            unimplemented!()
        }

        fn broadcast(&self, _: &Transaction) -> std::result::Result<(), Error> {
            unimplemented!()
        }

        fn estimate_fee(&self, _: usize) -> std::result::Result<FeeRate, Error> {
            unimplemented!()
        }
    }

    struct DummyNodeStorage;

    impl Storage for DummyNodeStorage {
        fn insert_payment(
            &self,
            _payment_hash: lightning::ln::PaymentHash,
            _info: crate::PaymentInfo,
        ) -> Result<()> {
            unimplemented!();
        }

        fn merge_payment(
            &self,
            _payment_hash: &lightning::ln::PaymentHash,
            _flow: crate::PaymentFlow,
            _amt_msat: crate::MillisatAmount,
            _fee_msat: crate::MillisatAmount,
            _htlc_status: crate::HTLCStatus,
            _preimage: Option<lightning::ln::PaymentPreimage>,
            _secret: Option<lightning::ln::PaymentSecret>,
            _funding_txid: Option<Txid>,
        ) -> Result<()> {
            unimplemented!();
        }

        fn get_payment(
            &self,
            _payment_hash: &lightning::ln::PaymentHash,
        ) -> Result<Option<(lightning::ln::PaymentHash, crate::PaymentInfo)>> {
            unimplemented!();
        }

        fn all_payments(&self) -> Result<Vec<(lightning::ln::PaymentHash, crate::PaymentInfo)>> {
            unimplemented!();
        }

        fn insert_spendable_output(
            &self,
            _descriptor: lightning::sign::SpendableOutputDescriptor,
        ) -> Result<()> {
            unimplemented!();
        }

        fn get_spendable_output(
            &self,
            _outpoint: &lightning::chain::transaction::OutPoint,
        ) -> Result<Option<lightning::sign::SpendableOutputDescriptor>> {
            unimplemented!();
        }

        fn delete_spendable_output(
            &self,
            _outpoint: &lightning::chain::transaction::OutPoint,
        ) -> Result<()> {
            unimplemented!();
        }

        fn all_spendable_outputs(&self) -> Result<Vec<lightning::sign::SpendableOutputDescriptor>> {
            unimplemented!();
        }

        fn upsert_channel(&self, _channel: Channel) -> Result<()> {
            unimplemented!();
        }

        fn get_channel(&self, _user_channel_id: &str) -> Result<Option<Channel>> {
            unimplemented!();
        }

        fn all_non_pending_channels(&self) -> Result<Vec<Channel>> {
            unimplemented!();
        }

        fn get_announced_channel(
            &self,
            _counterparty_pubkey: PublicKey,
        ) -> Result<Option<Channel>> {
            unimplemented!();
        }

        fn get_channel_by_payment_hash(&self, _payment_hash: String) -> Result<Option<Channel>> {
            unimplemented!();
        }

        fn upsert_transaction(&self, _transaction: crate::transaction::Transaction) -> Result<()> {
            unimplemented!();
        }

        fn get_transaction(&self, _txid: &str) -> Result<Option<crate::transaction::Transaction>> {
            unimplemented!();
        }

        fn all_transactions_without_fees(&self) -> Result<Vec<crate::transaction::Transaction>> {
            unimplemented!();
        }
    }
}
