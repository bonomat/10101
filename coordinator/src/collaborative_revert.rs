use crate::db;
use crate::db::positions::Position;
use crate::message::OrderbookMessage;
use crate::node::storage::NodeStorage;
use crate::position;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use axum::Json;
use bdk::bitcoin::Transaction;
use bitcoin::Amount;
use coordinator_commons::CollaborativeRevert;
use coordinator_commons::CollaborativeRevertData;
use diesel::r2d2::ConnectionManager;
use diesel::r2d2::Pool;
use diesel::r2d2::PooledConnection;
use diesel::PgConnection;
use dlc::util::weight_to_fee;
use dlc_manager::subchannel::LNChannelManager;
use ln_dlc_node::node::Node;
use orderbook_commons::Message;
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use trade::bitmex_client::Quote;

/// The weight for the collaborative close transaction. It's expected to have 1 input (from the fund
/// transaction) and 2 outputs, one for each party.
/// Note: if either party would have a 0 output, the actual weight will be smaller and we will be
/// overspending tx fee.
const COLLABORATIVE_REVERT_TX_WEIGHT: usize = 672;

pub async fn notify_user_to_collaboratively_revert(
    revert_params: Json<CollaborativeRevert>,
    channel_id_string: String,
    channel_id: [u8; 32],
    pool: Pool<ConnectionManager<PgConnection>>,
    node: Arc<Node<NodeStorage>>,
    auth_users_notifier: mpsc::Sender<OrderbookMessage>,
) -> anyhow::Result<()> {
    let mut conn = pool.get().context("Could not acquire db lock")?;

    let channel_details = node
        .channel_manager
        .get_channel_details(&channel_id)
        .context("Could not get channel")?;

    let sub_channels = node
        .list_dlc_channels()
        .context("Could not list dlc channels")?;

    let sub_channel = sub_channels
        .iter()
        .find(|c| c.channel_id == channel_id)
        .context("Could not find provided channel")?;

    let position =
        Position::get_position_by_trader(&mut conn, channel_details.counterparty.node_id, vec![])?
            .context("Could not load position for channel_id")?;

    let settlement_amount = position
        .calculate_settlement_amount(revert_params.price)
        .context("Could not calculate settlement amount")?;

    let pnl = position
        .calculate_coordinator_pnl(Quote {
            bid_size: 0,
            ask_size: 0,
            bid_price: revert_params.price,
            ask_price: revert_params.price,
            symbol: "".to_string(),
            timestamp: OffsetDateTime::now_utc(),
        })
        .context("Could not calculate coordinator pnl")?;

    // There is no easy way to get the total tx fee for all subchannel transactions, hence, we
    // estimate it. This transaction fee is shared among both users fairly
    let dlc_channel_fee = calculate_dlc_channel_tx_fees(
        sub_channel.fund_value_satoshis,
        pnl,
        channel_details.inbound_capacity_msat / 1000,
        channel_details.outbound_capacity_msat / 1000,
        position.trader_margin,
        position.coordinator_margin,
    )?;

    // Coordinator's amount is the total channel's value (fund_value_satoshis) whatever the taker
    // had (inbound_capacity), the taker's PnL (settlement_amount) and the transaction fee
    let coordinator_amount = sub_channel.fund_value_satoshis as i64
        - (channel_details.inbound_capacity_msat / 1000) as i64
        - settlement_amount as i64
        - (dlc_channel_fee as f64 / 2.0) as i64;
    let trader_amount = sub_channel.fund_value_satoshis - coordinator_amount as u64;

    let fee = weight_to_fee(
        COLLABORATIVE_REVERT_TX_WEIGHT,
        revert_params.fee_rate_sats_vb,
    )
    .expect("To be able to calculate constant fee rate");

    tracing::debug!(
        coordinator_amount,
        fund_value_satoshis = sub_channel.fund_value_satoshis,
        inbound_capacity_msat = channel_details.inbound_capacity_msat,
        settlement_amount,
        dlc_channel_fee,
        inbound_capacity_msat = channel_details.inbound_capacity_msat,
        outbound_capacity_msat = channel_details.outbound_capacity_msat,
        trader_margin = position.trader_margin,
        coordinator_margin = position.coordinator_margin,
        position_id = position.id,
        "Collaborative revert temporary values"
    );

    let coordinator_addrss = node.get_unused_address();
    let coordinator_amount = Amount::from_sat(coordinator_amount as u64 - fee / 2);
    let trader_amount = Amount::from_sat(trader_amount - fee / 2);

    // TODO: check if trader still has more than dust
    tracing::info!(
        channel_id = channel_id_string,
        coordinator_address = %coordinator_addrss,
        coordinator_amount = coordinator_amount.to_sat(),
        trader_amount = trader_amount.to_sat(),
        "Proposing collaborative revert");

    db::collaborative_reverts::insert(
        &mut conn,
        position::models::CollaborativeRevert {
            channel_id,
            trader_pubkey: position.trader,
            price: revert_params.price.to_f32().expect("to fit into f32"),
            coordinator_address: coordinator_addrss.clone(),
            coordinator_amount_sats: coordinator_amount,
            trader_amount_sats: trader_amount,
            timestamp: OffsetDateTime::now_utc(),
        },
    )
    .context("Could not insert new collaborative revert")?;

    // try to notify user
    let sender = auth_users_notifier;
    sender
        .send(OrderbookMessage::CollaborativeRevert {
            trader_id: position.trader,
            message: Message::CollaborativeRevert {
                channel_id,
                coordinator_address: coordinator_addrss,
                coordinator_amount,
                trader_amount,
            },
        })
        .await
        .map_err(|error| anyhow!("Could send message to notify user {error:#}"))?;
    Ok(())
}

fn calculate_dlc_channel_tx_fees(
    initial_funding: u64,
    pnl: i64,
    inbound_capacity: u64,
    outbound_capacity: u64,
    trader_margin: i64,
    coordinator_margin: i64,
) -> anyhow::Result<u64> {
    let dlc_tx_fee = initial_funding
        .checked_sub(inbound_capacity)
        .context("could not subtract inbound capacity")?
        .checked_sub(outbound_capacity)
        .context("could not subtract outbound capacity")?
        .checked_sub(
            trader_margin
                .checked_sub(pnl)
                .context("could not substract pnl")? as u64,
        )
        .context("could not subtract trader margin")?
        .checked_sub(
            coordinator_margin
                .checked_add(pnl)
                .context("could not add pnl")? as u64,
        )
        .context("could not subtract coordinator margin")?;
    Ok(dlc_tx_fee)
}

#[cfg(test)]
pub mod tests {
    use crate::collaborative_revert::calculate_dlc_channel_tx_fees;

    #[test]
    pub fn calculate_transaction_fee_for_dlc_channel_transactions() {
        let total_fee =
            calculate_dlc_channel_tx_fees(200_000, -4047, 65_450, 85_673, 18_690, 18_690).unwrap();
        assert_eq!(total_fee, 11_497);
    }

    #[test]
    pub fn ensure_overflow_being_caught() {
        assert!(
            calculate_dlc_channel_tx_fees(200_000, -100, 65_383, 88_330, 180_362, 180_362).is_err()
        );
    }
}

pub fn confirm_collaborative_revert(
    revert_params: &Json<CollaborativeRevertData>,
    conn: &mut PooledConnection<ConnectionManager<PgConnection>>,
    channel_id: [u8; 32],
    inner_node: Arc<Node<NodeStorage>>,
) -> anyhow::Result<Transaction> {
    // TODO: check if provided amounts are as expected
    if !revert_params
        .transaction
        .output
        .iter()
        .any(|output| inner_node.wallet().is_mine(&output.script_pubkey).is_ok())
    {
        let error_message = "Invalid request: no address for coordinator provided".to_string();
        tracing::error!(error_message);
        bail!(error_message);
    }

    let sub_channels = inner_node
        .list_dlc_channels()
        .context("Failed to list dlc channels")?;
    let sub_channel = sub_channels
        .iter()
        .find(|c| c.channel_id == channel_id)
        .context("Could not find provided channel")?;

    let mut revert_transaction = revert_params.transaction.clone();

    let position = Position::get_position_by_trader(conn, sub_channel.counter_party, vec![])?
        .context("Could not load position for channel_id")?;

    let signature = inner_node
        .sub_channel_manager
        .get_holder_split_tx_signature(sub_channel, &revert_transaction)
        .context("Could not sign transaction")?;

    dlc::util::finalize_multi_sig_input_transaction(
        &mut revert_transaction,
        vec![
            (sub_channel.own_fund_pk, signature),
            (sub_channel.counter_fund_pk, revert_params.signature),
        ],
        &sub_channel.original_funding_redeemscript,
        0,
    );

    // if we have a sig here, it means we were able to sign the transaction and can broadcast it
    inner_node
        .wallet()
        .broadcast_transaction(&revert_transaction)
        .context("Could not broadcast transaction")?;

    Position::set_position_to_closed(conn, position.id)
        .context("Could not set position to closed")?;

    Ok(revert_transaction)
}
