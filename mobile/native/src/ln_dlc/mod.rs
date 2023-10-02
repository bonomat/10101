use self::node::WalletHistories;
use crate::api;
use crate::api::PaymentFlow;
use crate::api::Status;
use crate::api::WalletHistoryItem;
use crate::api::WalletHistoryItemType;
use crate::calculations;
use crate::channel_fee::ChannelFeePaymentSubscriber;
use crate::commons::reqwest_client;
use crate::config;
use crate::event;
use crate::event::EventInternal;
use crate::ln_dlc::channel_status::track_channel_status;
use crate::ln_dlc::node::Node;
use crate::ln_dlc::node::NodeStorage;
use crate::trade::order;
use crate::trade::order::FailureReason;
use crate::trade::position;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use bdk::bitcoin::secp256k1::rand::thread_rng;
use bdk::bitcoin::secp256k1::rand::RngCore;
use bdk::bitcoin::secp256k1::SecretKey;
use bdk::bitcoin::Txid;
use bdk::bitcoin::XOnlyPublicKey;
use bdk::BlockTime;
use bdk::FeeRate;
use bitcoin::hashes::hex::ToHex;
use bitcoin::Amount;
use coordinator_commons::LspConfig;
use coordinator_commons::TradeParams;
use itertools::chain;
use itertools::Itertools;
use lightning::ln::channelmanager::ChannelDetails;
use lightning::util::events::Event;
use lightning_invoice::Invoice;
use ln_dlc_node::channel::JIT_FEE_INVOICE_DESCRIPTION_PREFIX;
use ln_dlc_node::config::app_config;
use ln_dlc_node::node::rust_dlc_manager::subchannel::LNChannelManager;
use ln_dlc_node::node::rust_dlc_manager::subchannel::SubChannelState;
use ln_dlc_node::node::rust_dlc_manager::ChannelId;
use ln_dlc_node::node::rust_dlc_manager::Storage as DlcStorage;
use ln_dlc_node::node::LnDlcNodeSettings;
use ln_dlc_node::node::NodeInfo;
use ln_dlc_node::scorer;
use ln_dlc_node::seed::Bip39Seed;
use ln_dlc_node::util;
use ln_dlc_node::AppEventHandler;
use ln_dlc_node::HTLCStatus;
use ln_dlc_node::CONFIRMATION_TARGET;
use orderbook_commons::RouteHintHop;
use orderbook_commons::FEE_INVOICE_DESCRIPTION_PREFIX_TAKER;
use rust_decimal::Decimal;
use state::Storage;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::spawn_blocking;
use trade::Direction;

mod lightning_subscriber;
mod node;
mod sync_position_to_dlc;

pub mod channel_status;

pub use channel_status::ChannelStatus;

const PROCESS_INCOMING_DLC_MESSAGES_INTERVAL: Duration = Duration::from_millis(200);
const UPDATE_WALLET_HISTORY_INTERVAL: Duration = Duration::from_secs(5);
const CHECK_OPEN_ORDERS_INTERVAL: Duration = Duration::from_secs(60);
const ON_CHAIN_SYNC_INTERVAL: Duration = Duration::from_secs(300);

/// The weight estimate of the funding transaction
///
/// This weight estimate assumes two inputs.
/// This value was chosen based on mainnet channel funding transactions with two inputs.
/// Note that we cannot predict this value precisely, because the app cannot predict what UTXOs the
/// coordinator will use for the channel opening transaction. Only once the transaction is know the
/// exact fee will be know.
pub const FUNDING_TX_WEIGHT_ESTIMATE: u64 = 220;

static NODE: Storage<Arc<Node>> = Storage::new();
static SEED: Storage<Bip39Seed> = Storage::new();

/// Trigger an on-chain sync followed by an update to the wallet balance and history.
///
/// We do not wait for the triggered task to finish, because the effect will be reflected
/// asynchronously on the UI.
pub async fn refresh_wallet_info() -> Result<()> {
    let node = NODE.try_get().context("failed to get ln dlc node")?;
    let wallet = node.inner.wallet();

    // Spawn into the blocking thread pool of the dedicated backend runtime to avoid blocking the UI
    // thread.
    let runtime = get_or_create_tokio_runtime()?;
    runtime.spawn_blocking(move || {
        if let Err(e) = wallet.sync() {
            tracing::error!("Manually triggered on-chain sync failed: {e:#}");
        }

        if let Err(e) = node.inner.sync_lightning_wallet() {
            tracing::error!("Manually triggered Lightning wallet sync failed: {e:#}");
        }

        if let Err(e) = keep_wallet_balance_and_history_up_to_date(node) {
            tracing::error!("Failed to keep wallet history up to date: {e:#}");
        }

        anyhow::Ok(())
    });

    Ok(())
}

pub fn get_seed_phrase() -> Vec<String> {
    SEED.try_get()
        .expect("SEED to be initialised")
        .get_seed_phrase()
}

pub fn get_node_key() -> SecretKey {
    NODE.get().inner.node_key()
}

pub fn get_node_info() -> Result<NodeInfo> {
    Ok(NODE
        .try_get()
        .context("NODE is not initialised yet, can't retrieve node info")?
        .inner
        .info)
}

pub async fn update_node_settings(settings: LnDlcNodeSettings) {
    let node = NODE.get();
    node.inner.update_settings(settings).await;
}

pub fn get_oracle_pubkey() -> XOnlyPublicKey {
    NODE.get().inner.oracle_pk()
}

pub fn get_funding_transaction(channel_id: &ChannelId) -> Result<Txid> {
    let node = NODE.get();
    let channel_details = node.inner.channel_manager.get_channel_details(channel_id);

    let funding_transaction = match channel_details {
        Some(channel_details) => match channel_details.funding_txo {
            Some(funding_txo) => funding_txo.txid,
            None => bail!(
                "Could not find funding transaction for channel {}",
                hex::encode(channel_id)
            ),
        },
        None => bail!(
            "Could not find channel details for {}",
            hex::encode(channel_id)
        ),
    };

    Ok(funding_transaction)
}

/// Lazily creates a multi threaded runtime with the the number of worker threads corresponding to
/// the number of available cores.
pub fn get_or_create_tokio_runtime() -> Result<&'static Runtime> {
    static RUNTIME: Storage<Runtime> = Storage::new();

    if RUNTIME.try_get().is_none() {
        let runtime = Runtime::new()?;
        RUNTIME.set(runtime);
    }

    Ok(RUNTIME.get())
}

/// Start the node
///
/// Allows specifying a data directory and a seed directory to decouple
/// data and seed storage (e.g. data is useful for debugging, seed location
/// should be more protected).
pub fn run(data_dir: String, seed_dir: String, runtime: &Runtime) -> Result<()> {
    let network = config::get_network();

    runtime.block_on(async move {
        event::publish(&EventInternal::Init("Starting full ldk node".to_string()));

        let mut ephemeral_randomness = [0; 32];
        thread_rng().fill_bytes(&mut ephemeral_randomness);

        let data_dir = Path::new(&data_dir).join(network.to_string());
        if !data_dir.exists() {
            std::fs::create_dir_all(&data_dir)
                .context(format!("Could not create data dir for {network}"))?;
        }

        // TODO: Consider using the same seed dir for all networks, and instead
        // change the filename, e.g. having `mainnet-seed` or `regtest-seed`
        let seed_dir = Path::new(&seed_dir).join(network.to_string());
        if !seed_dir.exists() {
            std::fs::create_dir_all(&seed_dir)
                .context(format!("Could not create data dir for {network}"))?;
        }

        event::subscribe(position::subscriber::Subscriber {});
        // TODO: Subscribe to events from the orderbook and publish OrderFilledWith event

        let address = {
            let listener = TcpListener::bind("0.0.0.0:0")?;
            listener.local_addr().expect("To get a free local address")
        };

        let seed_path = seed_dir.join("seed");
        let seed = Bip39Seed::initialize(&seed_path)?;
        SEED.set(seed.clone());

        let (event_sender, event_receiver) = watch::channel::<Option<Event>>(None);

        let node = ln_dlc_node::node::Node::new(
            app_config(),
            scorer::in_memory_scorer,
            "10101",
            network,
            data_dir.as_path(),
            Arc::new(NodeStorage),
            address,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), address.port()),
            util::into_net_addresses(address),
            config::get_esplora_endpoint(),
            seed,
            ephemeral_randomness,
            LnDlcNodeSettings::default(),
            config::get_oracle_info().into(),
        )?;
        let node = Arc::new(node);

        let event_handler = AppEventHandler::new(node.clone(), Some(event_sender));
        let _running = node.start(event_handler)?;
        let node = Arc::new(Node::new(node, _running));

        // Refresh the wallet balance and history eagerly so that it can complete before the
        // triggering the first on-chain sync. This ensures that the UI appears ready as soon as
        // possible.
        //
        // TODO: This might not be necessary once we rewrite the on-chain wallet with bdk:1.0.0.
        spawn_blocking({
            let node = node.clone();
            move || keep_wallet_balance_and_history_up_to_date(&node)
        })
        .await
        .expect("task to complete")?;

        runtime.spawn({
            let node = node.clone();
            async move {
                loop {
                    tokio::time::sleep(UPDATE_WALLET_HISTORY_INTERVAL).await;

                    let node = node.clone();
                    if let Err(e) =
                        spawn_blocking(move || keep_wallet_balance_and_history_up_to_date(&node))
                            .await
                            .expect("To spawn blocking task")
                    {
                        tracing::error!("Failed to sync balance and wallet history: {e:#}");
                    }
                }
            }
        });

        std::thread::spawn({
            let node = node.clone();
            move || loop {
                if let Err(e) = node.inner.sync_on_chain_wallet() {
                    tracing::error!("Failed on-chain sync: {e:#}");
                }

                std::thread::sleep(ON_CHAIN_SYNC_INTERVAL);
            }
        });

        runtime.spawn({
            let node = node.clone();
            async move { node.listen_for_lightning_events(event_receiver).await }
        });

        runtime.spawn({
            let node = node.clone();
            async move { node.keep_connected(config::get_coordinator_info()).await }
        });

        runtime.spawn({
            let node = node.clone();
            async move {
                loop {
                    let node = node.clone();
                    spawn_blocking(move || node.process_incoming_dlc_messages())
                        .await
                        .expect("To spawn blocking thread");
                    tokio::time::sleep(PROCESS_INCOMING_DLC_MESSAGES_INTERVAL).await;
                }
            }
        });

        runtime.spawn(async move {
            loop {
                if let Err(e) = spawn_blocking(order::handler::check_open_orders)
                    .await
                    .expect("To spawn blocking task")
                {
                    tracing::error!("Error while checking open orders: {e:#}");
                }

                tokio::time::sleep(CHECK_OPEN_ORDERS_INTERVAL).await;
            }
        });

        event::subscribe(ChannelFeePaymentSubscriber::new(
            node.inner.channel_manager.clone(),
        ));

        runtime.spawn(track_channel_status(node.clone()));

        if let Err(e) = node.sync_position_with_dlc_channel_state().await {
            tracing::error!("Failed to sync position with dlc channel state. Error: {e:#}");
        }

        NODE.set(node);

        event::publish(&EventInternal::Init("10101 is ready.".to_string()));

        Ok(())
    })
}

fn keep_wallet_balance_and_history_up_to_date(node: &Node) -> Result<()> {
    let wallet_balances = node
        .get_wallet_balances()
        .context("Failed to get wallet balances")?;

    let WalletHistories {
        on_chain,
        off_chain,
    } = node
        .get_wallet_histories()
        .context("Failed to get wallet histories")?;

    let blockchain_height = node.get_blockchain_height()?;
    let on_chain = on_chain.iter().map(|details| {
        let net_sats = details.received as i64 - details.sent as i64;

        let (flow, amount_sats) = if net_sats >= 0 {
            (PaymentFlow::Inbound, net_sats as u64)
        } else {
            (PaymentFlow::Outbound, net_sats.unsigned_abs())
        };

        let (timestamp, n_confirmations) = match details.confirmation_time {
            Some(BlockTime { timestamp, height }) => (
                timestamp,
                // This is calculated manually to avoid wasteful requests to esplora,
                // since we can just cache the blockchain height as opposed to fetching it for each
                // block as with `LnDlcWallet::get_transaction_confirmations`
                blockchain_height
                    .checked_sub(height as u64)
                    .unwrap_or_default(),
            ),

            None => {
                (
                    // Unconfirmed transactions should appear towards the top of the history
                    OffsetDateTime::now_utc().unix_timestamp() as u64,
                    0,
                )
            }
        };

        let status = if n_confirmations >= 3 {
            Status::Confirmed
        } else {
            Status::Pending
        };

        let wallet_type = WalletHistoryItemType::OnChain {
            txid: details.txid.to_string(),
            fee_sats: details.fee,
            confirmations: n_confirmations,
        };

        WalletHistoryItem {
            flow,
            amount_sats,
            timestamp,
            status,
            wallet_type,
        }
    });

    let off_chain = off_chain.iter().filter_map(|details| {
        tracing::trace!(details = %details, "Off-chain payment details");

        let amount_sats = match details.amount_msat {
            Some(msat) => msat / 1_000,
            // Skip payments that don't yet have an amount associated
            None => return None,
        };

        let decoded_invoice = match details.invoice.as_deref().map(Invoice::from_str) {
            Some(Ok(inv)) => {
                tracing::trace!(?inv, "Decoded invoice");
                Some(inv)
            }
            Some(Err(err)) => {
                tracing::warn!(%err, "Failed to deserialize invoice");
                None
            }
            None => None,
        };

        let expired = decoded_invoice
            .as_ref()
            .map(|inv| inv.is_expired())
            .unwrap_or(false);

        let status = match details.status {
            HTLCStatus::Pending if expired => Status::Expired,
            HTLCStatus::Pending => Status::Pending,
            HTLCStatus::Succeeded => Status::Confirmed,
            HTLCStatus::Failed => Status::Failed,
        };

        let flow = match details.flow {
            ln_dlc_node::PaymentFlow::Inbound => PaymentFlow::Inbound,
            ln_dlc_node::PaymentFlow::Outbound => PaymentFlow::Outbound,
        };

        let timestamp = details.timestamp.unix_timestamp() as u64;

        let payment_hash = hex::encode(details.payment_hash.0);

        let description = &details.description;
        let wallet_type = if let Some(order_id) =
            description.strip_prefix(FEE_INVOICE_DESCRIPTION_PREFIX_TAKER)
        {
            WalletHistoryItemType::OrderMatchingFee {
                order_id: order_id.to_string(),
                payment_hash,
            }
        } else if let Some(funding_txid) =
            description.strip_prefix(JIT_FEE_INVOICE_DESCRIPTION_PREFIX)
        {
            WalletHistoryItemType::JitChannelFee {
                funding_txid: funding_txid.to_string(),
                payment_hash,
            }
        } else {
            let expiry_timestamp = decoded_invoice
                .and_then(|inv| inv.timestamp().checked_add(inv.expiry_time()))
                .map(|time| OffsetDateTime::from(time).unix_timestamp() as u64);

            WalletHistoryItemType::Lightning {
                payment_hash,
                description: details.description.clone(),
                payment_preimage: details.preimage.clone(),
                invoice: details.invoice.clone(),
                fee_msat: details.fee_msat,
                expiry_timestamp,
            }
        };

        Some(WalletHistoryItem {
            flow,
            amount_sats,
            timestamp,
            status,
            wallet_type,
        })
    });

    let trades = derive_trades_from_filled_orders()?;

    let history = chain![on_chain, off_chain, trades]
        .sorted_by(|a, b| b.timestamp.cmp(&a.timestamp))
        .collect();

    let wallet_info = api::WalletInfo {
        balances: wallet_balances.into(),
        history,
    };

    event::publish(&EventInternal::WalletInfoUpdateNotification(wallet_info));

    Ok(())
}

fn derive_trades_from_filled_orders() -> Result<Vec<WalletHistoryItem>> {
    let mut trades = vec![];
    let orders =
        crate::db::get_filled_orders().context("Failed to get filled orders; skipping update")?;

    match orders.as_slice() {
        [first, tail @ ..] => {
            // The first filled order must be an outbound "payment", since coins need to leave the
            // Lightning wallet to open the first DLC channel.
            let flow = PaymentFlow::Outbound;
            let amount_sats = first
                .trader_margin()
                .expect("Filled order to have a margin");

            trades.push(WalletHistoryItem {
                flow,
                amount_sats,
                timestamp: first.creation_timestamp.unix_timestamp() as u64,
                status: Status::Confirmed, // TODO: Support other order/trade statuses
                wallet_type: WalletHistoryItemType::Trade {
                    order_id: first.id.to_string(),
                },
            });

            let mut total_contracts = match first.direction {
                Direction::Long => first.quantity,
                Direction::Short => -first.quantity,
            };
            let mut previous_order = first;
            for order in tail {
                use trade::Direction::*;
                let new_contracts = match order.direction {
                    Long => order.quantity,
                    Short => -order.quantity,
                };
                let updated_total_contracts = total_contracts + new_contracts;

                // Closing the position.
                if updated_total_contracts == 0.0 {
                    let open_order = previous_order;
                    let trader_margin = open_order
                        .trader_margin()
                        .expect("Filled order to have a margin");
                    let execution_price = Decimal::try_from(
                        order
                            .execution_price()
                            .expect("execution price to be set on a filled order"),
                    )?;

                    let opening_price = open_order
                        .execution_price()
                        .expect("initial execution price to be set on a filled order");

                    let pnl = calculations::calculate_pnl(
                        opening_price,
                        trade::Price {
                            ask: execution_price,
                            bid: execution_price,
                        },
                        open_order.quantity,
                        open_order.leverage,
                        open_order.direction,
                    )?;

                    // Closing a position is an inbound "payment", because the DLC channel is closed
                    // into the Lightning channel.
                    let flow = PaymentFlow::Inbound;
                    let amount_sats = (trader_margin as i64 + pnl) as u64;

                    trades.push(WalletHistoryItem {
                        flow,
                        amount_sats,
                        timestamp: order.creation_timestamp.unix_timestamp() as u64,
                        status: Status::Confirmed,
                        wallet_type: WalletHistoryItemType::Trade {
                            order_id: order.id.to_string(),
                        },
                    });
                }
                // Opening the position.
                else if total_contracts == 0.0 && updated_total_contracts != 0.0 {
                    // Closing a position is an outbound "payment", since coins need to leave the
                    // Lightning wallet to open a DLC channel.
                    let flow = PaymentFlow::Outbound;
                    let amount_sats = order
                        .trader_margin()
                        .expect("Filled order to have a margin");

                    trades.push(WalletHistoryItem {
                        flow,
                        amount_sats,
                        timestamp: order.creation_timestamp.unix_timestamp() as u64,
                        status: Status::Confirmed, // TODO: Support other order/trade statuses
                        wallet_type: WalletHistoryItemType::Trade {
                            order_id: order.id.to_string(),
                        },
                    });
                } else if total_contracts.signum() == updated_total_contracts.signum()
                    && updated_total_contracts.abs() > total_contracts.abs()
                {
                    debug_assert!(false, "extending the position is unimplemented");
                } else if total_contracts.signum() == updated_total_contracts.signum()
                    && updated_total_contracts.abs() < total_contracts.abs()
                {
                    debug_assert!(false, "reducing the position is unimplemented");
                } else {
                    // Changing position direction e.g. from 100 long to 50 short.
                    debug_assert!(false, "changing position direction is unimplemented");
                }

                total_contracts = updated_total_contracts;
                previous_order = order;
            }
        }
        [] => {
            // No trades.
        }
    }

    Ok(trades)
}

pub fn get_unused_address() -> String {
    NODE.get().inner.get_unused_address().to_string()
}

pub fn close_channel(is_force_close: bool) -> Result<()> {
    let node = NODE.try_get().context("failed to get ln dlc node")?;

    let channels = node.inner.list_channels();
    let channel_details = channels.first().context("No channel to close")?;

    node.inner
        .close_channel(channel_details.channel_id, is_force_close)?;

    Ok(())
}

pub fn get_usable_channel_details() -> Result<Vec<ChannelDetails>> {
    let node = NODE.try_get().context("failed to get ln dlc node")?;
    let channels = node.inner.list_usable_channels();

    Ok(channels)
}

pub fn get_fee_rate() -> Result<FeeRate> {
    let node = NODE.try_get().context("failed to get ln dlc node")?;
    Ok(node.inner.wallet().get_fee_rate(CONFIRMATION_TARGET))
}

/// Returns currently possible max channel value.
///
/// This is to be used when requesting a new channel from the LSP or when checking max tradable
/// amount
pub fn max_channel_value() -> Result<Amount> {
    let node = NODE.try_get().context("failed to get ln dlc node")?;
    if let Some(existing_channel) = node
        .inner
        .list_channels()
        .first()
        .map(|c| c.channel_value_satoshis)
    {
        Ok(Amount::from_sat(existing_channel))
    } else {
        let lsp_config = poll_lsp_config()?;
        tracing::info!(
            channel_value_sats = lsp_config.max_channel_value_satoshi,
            "Received channel config from LSP"
        );
        Ok(Amount::from_sat(lsp_config.max_channel_value_satoshi))
    }
}

fn poll_lsp_config() -> Result<LspConfig, Error> {
    let runtime = get_or_create_tokio_runtime()?;
    runtime.block_on(async {
        let client = reqwest_client();
        let response = client
            .get(format!(
                "http://{}/api/lsp/config",
                config::get_http_endpoint(),
            ))
            // timeout arbitrarily chosen
            .timeout(Duration::from_secs(3))
            .send()
            .await?;

        if !response.status().is_success() {
            let text = response.text().await?;
            bail!("Failed to fetch channel config from LSP: {text}")
        }

        let channel_config: LspConfig = response.json().await?;

        Ok(channel_config)
    })
}

pub fn contract_tx_fee_rate() -> Result<u64> {
    let node = NODE.try_get().context("failed to get ln dlc node")?;
    if let Some(fee_rate_per_vb) = node
        .inner
        .list_dlc_channels()?
        .first()
        .map(|c| c.fee_rate_per_vb)
    {
        Ok(fee_rate_per_vb)
    } else {
        let lsp_config = poll_lsp_config()?;
        tracing::info!(
            channel_value_sats = lsp_config.contract_tx_fee_rate,
            "Received channel config from LSP"
        );
        Ok(lsp_config.contract_tx_fee_rate)
    }
}

pub fn create_invoice(amount_sats: Option<u64>) -> Result<Invoice> {
    let runtime = get_or_create_tokio_runtime()?;

    runtime.block_on(async {
        let node = NODE.get();
        let client = reqwest_client();
        let response = client
            .post(format!(
                "http://{}/api/prepare_interceptable_payment/{}",
                config::get_http_endpoint(),
                node.inner.info.pubkey
            ))
            .send()
            .await?;

        if !response.status().is_success() {
            let text = response.text().await?;
            bail!("Failed to fetch fake scid from coordinator: {text}")
        }

        let final_route_hint_hop: RouteHintHop = response.json().await?;
        let final_route_hint_hop = final_route_hint_hop.into();

        tracing::info!(
            ?final_route_hint_hop,
            "Registered interest to open JIT channel with coordinator"
        );

        node.inner.create_interceptable_invoice(
            amount_sats,
            None,
            "Fund your 10101 wallet".to_string(),
            final_route_hint_hop,
        )
    })
}

pub fn send_payment(invoice: &str) -> Result<()> {
    let invoice = Invoice::from_str(invoice).context("Could not parse Invoice string")?;
    NODE.get().inner.send_payment(&invoice)
}

pub async fn trade(trade_params: TradeParams) -> Result<(), (FailureReason, Error)> {
    let client = reqwest_client();
    let response = client
        .post(format!("http://{}/api/trade", config::get_http_endpoint()))
        .json(&trade_params)
        .send()
        .await
        .context("Failed to register with coordinator")
        .map_err(|e| (FailureReason::TradeRequest, e))?;

    if !response.status().is_success() {
        let response_text = match response.text().await {
            Ok(text) => text,
            Err(err) => {
                format!("could not decode response {err:#}")
            }
        };
        return Err((
            FailureReason::TradeResponse,
            anyhow!("Could not post trade to coordinator: {response_text}"),
        ));
    }

    tracing::info!("Sent trade request to coordinator successfully");

    let order_matching_fee_invoice = response.text().await.map_err(|e| {
        (
            FailureReason::TradeResponse,
            anyhow!("Could not deserialize order-matching fee invoice: {e:#}"),
        )
    })?;
    let order_matching_fee_invoice: Invoice = order_matching_fee_invoice.parse().map_err(|e| {
        (
            FailureReason::TradeResponse,
            anyhow!("Could not parse order-matching fee invoice: {e:#}"),
        )
    })?;

    let payment_hash = *order_matching_fee_invoice.payment_hash();

    spawn_blocking(|| {
        *NODE.get().order_matching_fee_invoice.write() = Some(order_matching_fee_invoice);
    });

    tracing::info!(%payment_hash, "Registered order-matching fee invoice to be paid later");

    Ok(())
}

/// initiates the rollover protocol with the coordinator
pub async fn rollover() -> Result<()> {
    let node = NODE.get();

    let dlc_channels = node
        .inner
        .sub_channel_manager
        .get_dlc_manager()
        .get_store()
        .get_sub_channels()?;

    let dlc_channel = dlc_channels
        .into_iter()
        .find(|chan| {
            chan.counter_party == config::get_coordinator_info().pubkey
                && matches!(chan.state, SubChannelState::Signed(_))
        })
        .context("Couldn't find dlc channel to rollover")?;

    let dlc_channel_id = dlc_channel
        .get_dlc_channel_id(0)
        .context("Couldn't get dlc channel id")?;

    let client = reqwest_client();
    let response = client
        .post(format!(
            "http://{}/api/rollover/{}",
            config::get_http_endpoint(),
            dlc_channel_id.to_hex()
        ))
        .send()
        .await
        .with_context(|| format!("Failed to rollover dlc with id {}", dlc_channel_id.to_hex()))?;

    if !response.status().is_success() {
        let response_text = match response.text().await {
            Ok(text) => text,
            Err(err) => {
                format!("could not decode response {err:#}")
            }
        };

        bail!(
            "Failed to rollover dlc with id {}. Error: {response_text}",
            dlc_channel_id.to_hex()
        )
    }

    tracing::info!("Sent rollover request to coordinator successfully");

    Ok(())
}
