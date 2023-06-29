use crate::node::Node;
use crate::node::Storage;
use crate::MillisatAmount;
use crate::PaymentFlow;
use crate::PaymentInfo;
use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use autometrics::autometrics;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::Network;
use lightning::ln::channelmanager::Retry;
use lightning::ln::channelmanager::RetryableSendFailure;
use lightning::ln::channelmanager::MIN_CLTV_EXPIRY_DELTA;
use lightning::ln::PaymentHash;
use lightning::routing::gossip::RoutingFees;
use lightning::routing::router::RouteHint;
use lightning::routing::router::RouteHintHop;
use lightning_invoice::payment::pay_invoice;
use lightning_invoice::payment::PaymentError;
use lightning_invoice::Currency;
use lightning_invoice::Invoice;
use lightning_invoice::InvoiceBuilder;
use lightning_invoice::InvoiceDescription;
use std::time::Duration;
use std::time::SystemTime;
use time::OffsetDateTime;

impl<P> Node<P>
where
    P: Storage,
{
    #[autometrics]
    pub fn create_invoice(
        &self,
        amount_in_sats: u64,
        description: String,
        expiry: u32,
    ) -> Result<Invoice> {
        lightning_invoice::utils::create_invoice_from_channelmanager(
            &self.channel_manager,
            self.keys_manager.clone(),
            self.logger.clone(),
            self.get_currency(),
            Some(amount_in_sats * 1000),
            description,
            expiry,
            None,
        )
        .map_err(|e| anyhow!(e))
    }

    /// Creates an invoice which is meant to be intercepted
    ///
    /// Doing so we need to pass in `intercepted_channel_id` which needs to be generated by the
    /// intercepting node. This information, in combination with `hop_before_me` is used to add a
    /// routing hint to the invoice. Otherwise the sending node does not know how to pay the
    /// invoice.
    /// This is only used by the app to create the interchangeable invoice once we received the
    /// intercept scid from the coordinator,
    #[autometrics]
    pub fn create_interceptable_invoice(
        &self,
        amount_in_sats: Option<u64>,
        intercepted_channel_id: u64,
        hop_before_me: PublicKey,
        invoice_expiry: u32,
        description: String,
        proportional_fee_millionth: u32,
    ) -> Result<Invoice> {
        let amount_msat = amount_in_sats.map(|x| x * 1000);
        let (payment_hash, payment_secret) = self
            .channel_manager
            .create_inbound_payment(amount_msat, invoice_expiry, None)
            .map_err(|_| anyhow!("Failed to create inbound payment"))?;
        let invoice_builder = InvoiceBuilder::new(self.get_currency())
            .payee_pub_key(self.info.pubkey)
            .description(description)
            .payment_hash(sha256::Hash::from_slice(&payment_hash.0)?)
            .payment_secret(payment_secret)
            .timestamp(SystemTime::now())
            // lnd defaults the min final cltv to 9 (according to BOLT 11 - the recommendation has
            // changed to 18) 9 is not safe to use for ldk, because ldk mandates that
            // the `cltv_expiry_delta` has to be greater than `HTLC_FAIL_BACK_BUFFER`
            // (23).
            .min_final_cltv_expiry_delta(MIN_CLTV_EXPIRY_DELTA.into())
            .private_route(RouteHint(vec![RouteHintHop {
                src_node_id: hop_before_me,
                short_channel_id: intercepted_channel_id,
                // QUESTION: What happens if these differ with the actual values
                // in the `ChannelConfig` for the private channel?
                fees: RoutingFees {
                    base_msat: 1000,
                    proportional_millionths: proportional_fee_millionth,
                },
                cltv_expiry_delta: MIN_CLTV_EXPIRY_DELTA,
                htlc_minimum_msat: None,
                htlc_maximum_msat: None,
            }]));

        let invoice_builder = match amount_msat {
            Some(msats) => invoice_builder.amount_milli_satoshis(msats),
            None => invoice_builder,
        };

        let node_secret = self.keys_manager.get_node_secret_key();

        let signed_invoice = invoice_builder
            .build_raw()?
            .sign::<_, ()>(|hash| {
                let secp_ctx = Secp256k1::new();
                Ok(secp_ctx.sign_ecdsa_recoverable(hash, &node_secret))
            })
            .map_err(|_| anyhow!("Failed to sign invoice"))?;
        let invoice = Invoice::from_signed(signed_invoice)?;
        Ok(invoice)
    }

    fn get_currency(&self) -> Currency {
        match self.network {
            Network::Bitcoin => Currency::Bitcoin,
            Network::Testnet => Currency::BitcoinTestnet,
            Network::Regtest => Currency::Regtest,
            Network::Signet => Currency::Signet,
        }
    }

    /// Creates a fake channel id needed to intercept payments to the provided `target_node`
    ///
    /// This is mainly used for instant payments where the receiver does not have a lightning
    /// channel yet, e.g. Alice does not have a channel with Bob yet but wants to
    /// receive a LN payment. Clair pays to Bob who opens a channel to Alice and pays her.
    ///
    /// - `jit_fee_rate_basis_points`
    /// Fee rate to be charged for opening just in time channels. Rate is in basis points, i.e.
    /// 100 basis point=1% or 50=0.5%
    #[autometrics]
    pub fn create_intercept_scid(
        &self,
        target_node: PublicKey,
        jit_fee_rate_basis_point: u32,
    ) -> InterceptableScidDetails {
        let intercept_scid = self.channel_manager.get_intercept_scid();
        self.fake_channel_payments
            .lock()
            .expect("Mutex to not be poisoned")
            .insert(intercept_scid, target_node);

        tracing::info!(peer_id=%target_node, %intercept_scid, "Successfully created intercept scid for payment routing");
        InterceptableScidDetails {
            scid: intercept_scid,
            jit_routing_fee_millionth: jit_fee_rate_basis_point * 100,
        }
    }

    #[autometrics]
    pub fn send_payment(&self, invoice: &Invoice) -> Result<()> {
        let (status, err) = match pay_invoice(invoice, Retry::Attempts(10), &self.channel_manager) {
            Ok(_) => {
                let payee_pubkey = match invoice.payee_pub_key() {
                    Some(pubkey) => *pubkey,
                    None => invoice.recover_payee_pub_key(),
                };

                let amt_msat = invoice
                    .amount_milli_satoshis()
                    .context("invalid msat amount in the invoice")?;
                tracing::info!(peer_id=%payee_pubkey, "EVENT: initiated sending {amt_msat} msats",);
                (HTLCStatus::Pending, None)
            }
            Err(PaymentError::Invoice(err)) => {
                tracing::error!(%err, "Invalid invoice");
                anyhow::bail!(err);
            }
            Err(PaymentError::Sending(err)) => {
                tracing::error!(?err, "Failed to send payment");
                let failure_reason = retryable_send_failure_to_string(err);

                (HTLCStatus::Failed, Some(failure_reason))
            }
        };

        let description = match invoice.description() {
            InvoiceDescription::Direct(des) => des.clone().into_inner(),
            InvoiceDescription::Hash(lightning_invoice::Sha256(des)) => des.to_string(),
        };

        self.storage.insert_payment(
            PaymentHash(invoice.payment_hash().into_inner()),
            PaymentInfo {
                preimage: None,
                secret: None,
                status,
                amt_msat: MillisatAmount(invoice.amount_milli_satoshis()),
                flow: PaymentFlow::Outbound,
                timestamp: OffsetDateTime::now_utc(),
                description,
            },
        )?;

        if let Some(failure_reason) = err {
            anyhow::bail!("Failed to send payment: {}, {}", failure_reason, invoice);
        }

        Ok(())
    }

    #[cfg(test)]
    pub async fn wait_for_payment_claimed(
        &self,
        hash: &sha256::Hash,
    ) -> Result<(), tokio::time::error::Elapsed> {
        self.wait_for_payment(HTLCStatus::Succeeded, hash).await
    }

    #[autometrics]
    pub async fn wait_for_payment(
        &self,
        expected_status: HTLCStatus,
        hash: &sha256::Hash,
    ) -> Result<(), tokio::time::error::Elapsed> {
        assert_ne!(
            expected_status,
            HTLCStatus::Pending,
            "Waiting for pending is not a valid status"
        );
        let payment_hash = PaymentHash(hash.into_inner());

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                match self.storage.get_payment(&payment_hash) {
                    Ok(Some((_, PaymentInfo { status, .. }))) => {
                        tracing::debug!(
                            payment_hash = %hex::encode(hash),
                            ?status,
                            "Checking if payment has been claimed"
                        );
                        if expected_status == status {
                            return;
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(
                            payment_hash = %hex::encode(hash),
                            status = "unknown",
                            "Checking if payment has been claimed"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            payment_hash = %hex::encode(hash),
                            status = "error",
                            "Can't access store to load payment: {e:#}"
                        );
                    }
                }
            }
        })
        .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HTLCStatus {
    Pending,
    Succeeded,
    Failed,
}

pub struct InterceptableScidDetails {
    pub scid: u64,
    pub jit_routing_fee_millionth: u32,
}

fn retryable_send_failure_to_string(failure: RetryableSendFailure) -> &'static str {
    match failure {
        RetryableSendFailure::DuplicatePayment => "Duplicate payment",
        RetryableSendFailure::PaymentExpired => "Payment expired",
        RetryableSendFailure::RouteNotFound => "Route not found",
    }
}
