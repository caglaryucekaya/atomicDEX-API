use super::*;
use bitcoin::blockdata::script::Script;
use bitcoin::blockdata::transaction::Transaction;
use common::executor::{spawn, Timer};
use common::{log, now_ms};
use core::time::Duration;
use futures::compat::Future01CompatExt;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::keysinterface::SpendableOutputDescriptor;
use lightning::util::events::{Event, EventHandler, PaymentPurpose};
use rand::Rng;
use script::{Builder, SignatureVersion};
use secp256k1::Secp256k1;
use std::collections::hash_map::Entry;
use std::convert::TryFrom;
use std::sync::Arc;
use utxo_signer::with_key_pair::sign_tx;

pub struct LightningEventHandler {
    platform: Arc<Platform>,
    channel_manager: Arc<ChannelManager>,
    keys_manager: Arc<KeysManager>,
    persister: Arc<LightningPersister>,
    inbound_payments: PaymentsMapShared,
    outbound_payments: PaymentsMapShared,
}

impl EventHandler for LightningEventHandler {
    fn handle_event(&self, event: &Event) {
        match event {
            Event::FundingGenerationReady {
                temporary_channel_id,
                channel_value_satoshis,
                output_script,
                user_channel_id,
            } => self.handle_funding_generation_ready(*temporary_channel_id, *channel_value_satoshis, output_script, *user_channel_id),
            Event::PaymentReceived {
                payment_hash,
                amt,
                purpose,
            } => self.handle_payment_received(*payment_hash, *amt, purpose),
            Event::PaymentSent {
                payment_preimage,
                payment_hash,
                fee_paid_msat,
                ..
            } => self.handle_payment_sent(*payment_preimage, *payment_hash, *fee_paid_msat),
            Event::PaymentFailed { payment_hash, .. } => self.handle_payment_failed(payment_hash),
            Event::PendingHTLCsForwardable { time_forwardable } => self.handle_pending_htlcs_forwards(*time_forwardable),
            Event::SpendableOutputs { outputs } => self.handle_spendable_outputs(outputs),
            // Todo: an RPC for total amount earned
            Event::PaymentForwarded { fee_earned_msat, claim_from_onchain_tx } => log::info!(
                    "Recieved a fee of {} milli-satoshis for a successfully forwarded payment through our {} lightning node. Was the forwarded HTLC claimed by our counterparty via an on-chain transaction?: {}",
                    fee_earned_msat.unwrap_or_default(),
                    self.platform.coin.ticker(),
                    claim_from_onchain_tx,
                ),
            Event::ChannelClosed { channel_id, user_channel_id, reason } => self.handle_channel_closed(*channel_id, *user_channel_id, reason.to_string()),
            // Todo: Add spent UTXOs to RecentlySpentOutPoints if it's not discarded
            Event::DiscardFunding { channel_id, transaction } => log::info!(
                    "Discarding funding tx: {} for channel {}",
                    transaction.txid().to_string(),
                    hex::encode(channel_id),
                ),
            // Handling updating channel penalties after successfully routing a payment along a path is done by the InvoicePayer.
            Event::PaymentPathSuccessful {
                payment_id,
                payment_hash,
                path,
            } => log::info!(
                "Payment path: {:?}, successful for payment hash: {}, payment id: {}",
                path.iter().map(|hop| hop.pubkey.to_string()).collect::<Vec<_>>(),
                payment_hash.map(|h| hex::encode(h.0)).unwrap_or_default(),
                hex::encode(payment_id.0)
            ),
            // Handling updating channel penalties after a payment fails to route through a channel is done by the InvoicePayer.
            // Also abandoning or retrying a payment is handled by the InvoicePayer. 
            Event::PaymentPathFailed { payment_hash, rejected_by_dest, all_paths_failed, path, .. } => log::info!(
                "Payment path: {:?}, failed for payment hash: {}, Was rejected by destination?: {}, All paths failed?: {}",
                path.iter().map(|hop| hop.pubkey.to_string()).collect::<Vec<_>>(),
                hex::encode(payment_hash.0),
                rejected_by_dest,
                all_paths_failed,
            ),
        }
    }
}

// Generates the raw funding transaction with one output equal to the channel value.
fn sign_funding_transaction(
    user_channel_id: u64,
    output_script: &Script,
    platform: Arc<Platform>,
) -> OpenChannelResult<Transaction> {
    let coin = &platform.coin;
    let mut unsigned = {
        let unsigned_funding_txs = platform.unsigned_funding_txs.lock();
        unsigned_funding_txs
            .get(&user_channel_id)
            .ok_or_else(|| {
                OpenChannelError::InternalError(format!(
                    "Unsigned funding tx not found for internal channel id: {}",
                    user_channel_id
                ))
            })?
            .clone()
    };
    unsigned.outputs[0].script_pubkey = output_script.to_bytes().into();

    let my_address = coin.as_ref().derivation_method.iguana_or_err()?;
    let key_pair = coin.as_ref().priv_key_policy.key_pair_or_err()?;

    let prev_script = Builder::build_p2pkh(&my_address.hash);
    let signed = sign_tx(
        unsigned,
        key_pair,
        prev_script,
        SignatureVersion::WitnessV0,
        coin.as_ref().conf.fork_id,
    )?;

    Transaction::try_from(signed).map_to_mm(|e| OpenChannelError::ConvertTxErr(e.to_string()))
}

impl LightningEventHandler {
    pub fn new(
        platform: Arc<Platform>,
        channel_manager: Arc<ChannelManager>,
        keys_manager: Arc<KeysManager>,
        persister: Arc<LightningPersister>,
        inbound_payments: PaymentsMapShared,
        outbound_payments: PaymentsMapShared,
    ) -> Self {
        LightningEventHandler {
            platform,
            channel_manager,
            keys_manager,
            persister,
            inbound_payments,
            outbound_payments,
        }
    }

    fn handle_funding_generation_ready(
        &self,
        temporary_channel_id: [u8; 32],
        channel_value_satoshis: u64,
        output_script: &Script,
        user_channel_id: u64,
    ) {
        log::info!(
            "Handling FundingGenerationReady event for internal channel id: {}",
            user_channel_id
        );
        let funding_tx = match sign_funding_transaction(user_channel_id, output_script, self.platform.clone()) {
            Ok(tx) => tx,
            Err(e) => {
                log::error!(
                    "Error generating funding transaction for internal channel id {}: {}",
                    user_channel_id,
                    e.to_string()
                );
                return;
            },
        };
        let funding_txid = funding_tx.txid();
        // Give the funding transaction back to LDK for opening the channel.
        if let Err(e) = self
            .channel_manager
            .funding_transaction_generated(&temporary_channel_id, funding_tx)
        {
            log::error!("{:?}", e);
            return;
        }
        let platform = self.platform.clone();
        let persister = self.persister.clone();
        spawn(async move {
            let current_block = platform.coin.current_block().compat().await.unwrap_or_default();
            persister
                .add_funding_tx_to_sql(
                    user_channel_id,
                    funding_txid.to_string(),
                    channel_value_satoshis,
                    current_block,
                )
                .await
                .error_log();
        });
    }

    fn handle_payment_received(&self, payment_hash: PaymentHash, amt: u64, purpose: &PaymentPurpose) {
        log::info!(
            "Handling PaymentReceived event for payment_hash: {}",
            hex::encode(payment_hash.0)
        );
        let (payment_preimage, payment_secret) = match purpose {
            PaymentPurpose::InvoicePayment {
                payment_preimage,
                payment_secret,
            } => match payment_preimage {
                Some(preimage) => (*preimage, Some(*payment_secret)),
                None => return,
            },
            PaymentPurpose::SpontaneousPayment(preimage) => (*preimage, None),
        };
        let status = match self.channel_manager.claim_funds(payment_preimage) {
            true => {
                log::info!(
                    "Received an amount of {} millisatoshis for payment hash {}",
                    amt,
                    hex::encode(payment_hash.0)
                );
                HTLCStatus::Succeeded
            },
            false => HTLCStatus::Failed,
        };
        let mut payments = self.inbound_payments.lock();
        match payments.entry(payment_hash) {
            Entry::Occupied(mut e) => {
                let payment = e.get_mut();
                payment.status = status;
                payment.preimage = Some(payment_preimage);
                payment.secret = payment_secret;
            },
            Entry::Vacant(e) => {
                e.insert(PaymentInfo {
                    preimage: Some(payment_preimage),
                    secret: payment_secret,
                    status,
                    amt_msat: Some(amt),
                    fee_paid_msat: None,
                });
            },
        }
    }

    fn handle_payment_sent(
        &self,
        payment_preimage: PaymentPreimage,
        payment_hash: PaymentHash,
        fee_paid_msat: Option<u64>,
    ) {
        log::info!(
            "Handling PaymentSent event for payment_hash: {}",
            hex::encode(payment_hash.0)
        );
        if let Some(payment) = self.outbound_payments.lock().get_mut(&payment_hash) {
            payment.preimage = Some(payment_preimage);
            payment.status = HTLCStatus::Succeeded;
            payment.fee_paid_msat = fee_paid_msat;
            log::info!(
                "Successfully sent payment of {} millisatoshis with payment hash {}",
                payment.amt_msat.unwrap_or_default(),
                hex::encode(payment_hash.0)
            );
        }
    }

    fn handle_channel_closed(&self, channel_id: [u8; 32], user_channel_id: u64, reason: String) {
        log::info!(
            "Channel: {} closed for the following reason: {}",
            hex::encode(channel_id),
            reason
        );
        let persister = self.persister.clone();
        let platform = self.platform.clone();
        // Todo: Handle inbound channels closure case after updating to latest version of rust-lightning
        // as it has a new OpenChannelRequest event where we can give an inbound channel a user_channel_id
        // other than 0 in sql
        if user_channel_id != 0 {
            spawn(async move {
                persister
                    .update_channel_to_closed(user_channel_id, reason)
                    .await
                    .error_log();
                if let Ok(Some(channel_details)) = persister
                    .get_channel_from_sql(user_channel_id)
                    .await
                    .error_log_passthrough()
                {
                    if let Some(tx_id) = channel_details.funding_tx {
                        if let Ok(tx_hash) = H256Json::from_str(&tx_id).error_log_passthrough() {
                            if let Ok(funding_tx_bytes) = platform
                                .coin
                                .as_ref()
                                .rpc_client
                                .get_transaction_bytes(&tx_hash)
                                .compat()
                                .await
                                .error_log_passthrough()
                            {
                                if let Ok(TransactionEnum::UtxoTx(closing_tx)) = platform
                                    .coin
                                    .wait_for_tx_spend(
                                        &funding_tx_bytes.into_vec(),
                                        (now_ms() / 1000) + 3600,
                                        channel_details.funding_generated_in_block.unwrap_or_default(),
                                        &None,
                                    )
                                    .compat()
                                    .await
                                    .error_log_passthrough()
                                {
                                    persister
                                        .add_closing_tx_to_sql(
                                            user_channel_id,
                                            closing_tx.hash().reversed().to_string(),
                                        )
                                        .await
                                        .error_log();
                                }
                            }
                        }
                    }
                }
            });
        }
    }

    fn handle_payment_failed(&self, payment_hash: &PaymentHash) {
        log::info!(
            "Handling PaymentFailed event for payment_hash: {}",
            hex::encode(payment_hash.0)
        );
        let mut outbound_payments = self.outbound_payments.lock();
        let outbound_payment = outbound_payments.get_mut(payment_hash);
        if let Some(payment) = outbound_payment {
            payment.status = HTLCStatus::Failed;
        }
    }

    fn handle_pending_htlcs_forwards(&self, time_forwardable: Duration) {
        log::info!("Handling PendingHTLCsForwardable event!");
        let min_wait_time = time_forwardable.as_millis() as u32;
        let channel_manager = self.channel_manager.clone();
        spawn(async move {
            let millis_to_sleep = rand::thread_rng().gen_range(min_wait_time, min_wait_time * 5);
            Timer::sleep_ms(millis_to_sleep).await;
            channel_manager.process_pending_htlc_forwards();
        });
    }

    fn handle_spendable_outputs(&self, outputs: &[SpendableOutputDescriptor]) {
        log::info!("Handling SpendableOutputs event!");
        let platform_coin = &self.platform.coin;
        // Todo: add support for Hardware wallets for funding transactions and spending spendable outputs (channel closing transactions)
        let my_address = match platform_coin.as_ref().derivation_method.iguana_or_err() {
            Ok(addr) => addr,
            Err(e) => {
                log::error!("{}", e);
                return;
            },
        };
        let change_destination_script = Builder::build_witness_script(&my_address.hash).to_bytes().take().into();
        let feerate_sat_per_1000_weight = self.platform.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
        let output_descriptors = &outputs.iter().collect::<Vec<_>>();
        let spending_tx = match self.keys_manager.spend_spendable_outputs(
            output_descriptors,
            Vec::new(),
            change_destination_script,
            feerate_sat_per_1000_weight,
            &Secp256k1::new(),
        ) {
            Ok(tx) => tx,
            Err(_) => {
                log::error!("Error spending spendable outputs");
                return;
            },
        };

        self.platform.broadcast_transaction(&spending_tx);

        for output in outputs {
            let (closing_txid, claimed_balance) = match output {
                SpendableOutputDescriptor::StaticOutput { outpoint, output } => {
                    (outpoint.txid.to_string(), output.value)
                },
                SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
                    (descriptor.outpoint.txid.to_string(), descriptor.output.value)
                },
                SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
                    (descriptor.outpoint.txid.to_string(), descriptor.output.value)
                },
            };
            let claiming_txid = spending_tx.txid().to_string();
            let persister = self.persister.clone();
            spawn(async move {
                persister
                    .add_claiming_tx_to_sql(closing_txid, claiming_txid, claimed_balance)
                    .await
                    .error_log();
            });
        }
    }
}
