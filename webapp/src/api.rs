use crate::subscribers::AppSubscribers;
use anyhow::Context;
use anyhow::Result;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Json;
use axum::Router;
use bitcoin::Amount;
use commons::order_matching_fee_taker;
use commons::Price;
use native::api::ContractSymbol;
use native::api::Direction;
use native::api::Fee;
use native::api::SendPayment;
use native::api::WalletHistoryItemType;
use native::calculations::calculate_pnl;
use native::ln_dlc;
use native::trade::order::FailureReason;
use native::trade::order::InvalidSubchannelOffer;
use native::trade::order::OrderType;
use native::trade::position::PositionState;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

pub fn router(subscribers: Arc<AppSubscribers>) -> Router {
    Router::new()
        .route("/api/balance", get(get_balance))
        .route("/api/newaddress", get(get_unused_address))
        .route("/api/sendpayment", post(send_payment))
        .route("/api/history", get(get_onchain_payment_history))
        .route("/api/orders", get(get_orders).post(post_new_order))
        .route("/api/positions", get(get_positions))
        .route("/api/quotes/:contract_symbol", get(get_best_quote))
        .route("/api/node", get(get_node_id))
        .route("/api/seed", get(get_seed_phrase))
        .with_state(subscribers)
}

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[derive(Serialize)]
pub struct Version {
    version: String,
    commit_hash: String,
    branch: String,
}

pub async fn version() -> Json<Version> {
    Json(Version {
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit_hash: env!("COMMIT_HASH").to_string(),
        branch: env!("BRANCH_NAME").to_string(),
    })
}

pub async fn get_unused_address() -> impl IntoResponse {
    ln_dlc::get_unused_address()
}

#[derive(Serialize)]
pub struct Balance {
    on_chain: u64,
    off_chain: u64,
}

pub async fn get_balance(
    State(subscribers): State<Arc<AppSubscribers>>,
) -> Result<Json<Option<Balance>>, AppError> {
    let balance = subscribers.wallet_info().map(|wallet_info| Balance {
        on_chain: wallet_info.balances.on_chain,
        off_chain: wallet_info.balances.off_chain,
    });

    Ok(Json(balance))
}

#[derive(Serialize)]
pub struct OnChainPayment {
    flow: String,
    amount: u64,
    timestamp: u64,
    txid: String,
    confirmations: u64,
    fee: Option<u64>,
}

pub async fn get_onchain_payment_history(
    State(subscribers): State<Arc<AppSubscribers>>,
) -> Result<Json<Vec<OnChainPayment>>, AppError> {
    let history = match subscribers.wallet_info() {
        Some(wallet_info) => wallet_info
            .history
            .into_iter()
            .filter_map(|item| match item.wallet_type {
                WalletHistoryItemType::OnChain {
                    txid,
                    fee_sats,
                    confirmations,
                } => Some(OnChainPayment {
                    flow: item.flow.to_string(),
                    amount: item.amount_sats,
                    timestamp: item.timestamp,
                    txid,
                    confirmations,
                    fee: fee_sats,
                }),
                _ => None,
            })
            .collect::<Vec<OnChainPayment>>(),
        None => vec![],
    };

    Ok(Json(history))
}

#[derive(Deserialize)]
pub struct Payment {
    address: String,
    amount: u64,
    fee: u64,
}

pub async fn send_payment(params: Json<Payment>) -> Result<(), AppError> {
    ln_dlc::send_payment(SendPayment::OnChain {
        address: params.0.address,
        amount: params.0.amount,
        fee: Fee::FeeRate { sats: params.0.fee },
    })
    .await?;

    ln_dlc::refresh_wallet_info().await?;
    Ok(())
}

pub async fn get_node_id() -> impl IntoResponse {
    ln_dlc::get_node_pubkey().to_string()
}

pub async fn get_seed_phrase() -> Json<Vec<String>> {
    Json(ln_dlc::get_seed_phrase())
}

#[derive(Serialize)]
pub struct OrderId {
    id: Uuid,
}

#[derive(Deserialize)]
pub struct NewOrderParams {
    #[serde(with = "rust_decimal::serde::float")]
    pub leverage: Decimal,
    #[serde(with = "rust_decimal::serde::float")]
    pub quantity: Decimal,
    pub direction: Direction,
}

impl TryFrom<NewOrderParams> for native::trade::order::Order {
    type Error = anyhow::Error;
    fn try_from(value: NewOrderParams) -> Result<Self> {
        Ok(native::trade::order::Order {
            id: Uuid::new_v4(),
            leverage: value
                .leverage
                .to_f32()
                .context("To be able to parse leverage into f32")?,
            quantity: value
                .quantity
                .to_f32()
                .context("To be able to parse leverage into f32")?,
            contract_symbol: ContractSymbol::BtcUsd,
            direction: value.direction,
            // We only support market orders for now
            order_type: OrderType::Market,
            state: native::trade::order::OrderState::Initial,
            creation_timestamp: OffsetDateTime::now_utc(),
            // We do not support setting order expiry from the frontend for now
            order_expiry_timestamp: OffsetDateTime::now_utc() + time::Duration::minutes(1),
            reason: native::trade::order::OrderReason::Manual,
            stable: false,
            failure_reason: None,
        })
    }
}

pub async fn post_new_order(params: Json<NewOrderParams>) -> Result<Json<OrderId>, AppError> {
    let order_id = native::trade::order::handler::submit_order(
        params
            .0
            .try_into()
            .context("Could not parse order request")?,
    )
    .await?;

    Ok(Json(OrderId { id: order_id }))
}

#[derive(Debug, Clone, Serialize)]
pub struct Position {
    pub leverage: f32,
    pub quantity: f32,
    pub contract_symbol: ContractSymbol,
    pub direction: Direction,
    pub average_entry_price: f32,
    pub liquidation_price: f32,
    pub position_state: PositionState,
    pub collateral: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub expiry: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
    pub stable: bool,
    pub pnl_sats: Option<i64>,
    #[serde(with = "bitcoin::util::amount::serde::as_sat::opt")]
    pub closing_fee: Option<Amount>,
}

impl From<(native::trade::position::Position, Option<Price>)> for Position {
    fn from((position, price): (native::trade::position::Position, Option<Price>)) -> Self {
        let res = price.map(|price| match (price.ask, price.bid) {
            (Some(ask), Some(bid)) => {
                let price = match position.direction {
                    Direction::Long => price.bid,
                    Direction::Short => price.ask,
                };

                (
                    calculate_pnl(
                        position.average_entry_price,
                        trade::Price { bid, ask },
                        position.quantity,
                        position.leverage,
                        position.direction,
                    )
                    .ok(),
                    price
                        .map(|price| Some(order_matching_fee_taker(position.quantity, price)))
                        .and_then(|price| price),
                )
            }
            _ => (None, None),
        });

        let (pnl_sats, closing_fee) = match res {
            None => (None, None),
            Some((pnl_sats, closing_fee)) => (pnl_sats, closing_fee),
        };

        Position {
            leverage: position.leverage,
            quantity: position.quantity,
            contract_symbol: position.contract_symbol,
            direction: position.direction,
            average_entry_price: position.average_entry_price,
            liquidation_price: position.liquidation_price,
            position_state: position.position_state,
            collateral: position.collateral,
            expiry: position.expiry,
            updated: position.updated,
            created: position.created,
            stable: position.stable,
            pnl_sats,
            closing_fee,
        }
    }
}

pub async fn get_positions(
    State(subscribers): State<Arc<AppSubscribers>>,
) -> Result<Json<Vec<Position>>, AppError> {
    let orderbook_info = subscribers.orderbook_info();

    let positions = native::trade::position::handler::get_positions()?
        .into_iter()
        .map(|position| {
            let quotes = orderbook_info
                .clone()
                .map(|prices| prices.get(&position.contract_symbol).cloned())
                .and_then(|inner| inner);
            (position, quotes).into()
        })
        .collect::<Vec<Position>>();

    Ok(Json(positions))
}

#[derive(Serialize, Debug)]
pub struct Order {
    pub id: Uuid,
    pub leverage: f32,
    pub quantity: f32,
    /// An order only has a price if it either was filled or if it was a limit order (which is not
    /// implemented yet).
    pub price: Option<f32>,
    pub contract_symbol: ContractSymbol,
    pub direction: Direction,
    pub order_type: OrderType,
    pub state: OrderState,
    #[serde(with = "time::serde::rfc3339")]
    pub creation_timestamp: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub order_expiry_timestamp: OffsetDateTime,
    pub failure_reason: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub enum OrderState {
    /// Not submitted to orderbook yet
    Initial,

    /// Rejected by the orderbook upon submission
    Rejected,

    /// Successfully submit to orderbook
    Open,

    /// The orderbook has matched the order and it is being filled
    Filling,

    /// The order failed to be filled
    Failed,

    /// Successfully set up trade
    Filled,
}

impl From<native::trade::order::OrderState> for OrderState {
    fn from(value: native::trade::order::OrderState) -> Self {
        match value {
            native::trade::order::OrderState::Initial => OrderState::Initial,
            native::trade::order::OrderState::Rejected => OrderState::Rejected,
            native::trade::order::OrderState::Open => OrderState::Open,
            native::trade::order::OrderState::Filling { .. } => OrderState::Filling,
            native::trade::order::OrderState::Failed { .. } => OrderState::Failed,
            native::trade::order::OrderState::Filled { .. } => OrderState::Filled,
        }
    }
}
impl From<&native::trade::order::Order> for Order {
    fn from(value: &native::trade::order::Order) -> Self {
        let failure_reason = match &value.failure_reason {
            None => None,
            Some(reason) => {
                let reason = match reason {
                    FailureReason::FailedToSetToFilling => "FailedToSetToFilling",
                    FailureReason::TradeRequest => "TradeRequestFailed",
                    FailureReason::TradeResponse(error) => error.as_str(),
                    FailureReason::CollabRevert => "CollabRevert",
                    FailureReason::OrderNotAcceptable => "OrderNotAcceptable",
                    FailureReason::TimedOut => "TimedOut",
                    FailureReason::InvalidDlcOffer(error) => match error {
                        InvalidSubchannelOffer::Outdated => "OfferOutdated",
                        InvalidSubchannelOffer::UndeterminedMaturityDate => {
                            "OfferUndeterminedMaturityDate"
                        }
                        InvalidSubchannelOffer::Unacceptable => "OfferUnacceptable",
                    },
                    FailureReason::OrderRejected => "OrderRejected",
                    FailureReason::Unknown => "Unknown",
                }
                .to_string();
                Some(reason)
            }
        };

        let mut price = None;

        if let OrderType::Limit { price: limit_price } = value.order_type {
            price.replace(limit_price);
        }

        // Note: we might overwrite a limit price here but this is not an issue because if a limit
        // order has been filled the limit price will be filled price and vice versa
        if let native::trade::order::OrderState::Filled { execution_price } = value.state {
            price.replace(execution_price);
        }

        Order {
            id: value.id,
            leverage: value.leverage,
            quantity: value.quantity,
            price,
            contract_symbol: value.contract_symbol,
            direction: value.direction,
            order_type: value.order_type,
            state: value.state.clone().into(),
            creation_timestamp: value.creation_timestamp,
            order_expiry_timestamp: value.order_expiry_timestamp,
            failure_reason,
        }
    }
}

pub async fn get_orders() -> Result<Json<Vec<Order>>, AppError> {
    let orders = native::trade::order::handler::get_orders_for_ui()
        .await?
        .iter()
        .map(|order| order.into())
        .collect();

    Ok(Json(orders))
}

pub async fn get_best_quote(
    State(subscribers): State<Arc<AppSubscribers>>,
    Path(contract_symbol): Path<ContractSymbol>,
) -> Result<Json<Option<Price>>, AppError> {
    let quotes = subscribers
        .orderbook_info()
        .map(|prices| prices.get(&contract_symbol).cloned())
        .and_then(|inner| inner);

    Ok(Json(quotes))
}
