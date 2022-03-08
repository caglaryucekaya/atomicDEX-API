use super::*;
use crate::lightning::ln_conf::{LightningCoinConf, LightningProtocolConf};
use crate::lightning::ln_p2p::{connect_to_nodes_loop, init_peer_manager, ln_node_announcement_loop};
use crate::utxo::rpc_clients::{electrum_script_hash, BestBlock as RpcBestBlock, ElectrumBlockHeader, ElectrumClient,
                               ElectrumNonce, UtxoRpcError};
use crate::utxo::utxo_standard::UtxoStandardCoin;
use crate::DerivationMethod;
use bitcoin::blockdata::block::BlockHeader;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode::deserialize;
use bitcoin::hash_types::{BlockHash, TxMerkleNode, Txid};
use bitcoin_hashes::{sha256d, Hash};
use common::executor::{spawn, Timer};
use common::jsonrpc_client::JsonRpcErrorType;
use common::log;
use common::log::LogState;
use common::mm_ctx::MmArc;
use futures::compat::Future01CompatExt;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager};
use lightning::chain::{chainmonitor, Access, BestBlock, Confirm, Watch};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager};
use lightning::routing::network_graph::{NetGraphMsgHandler, NetworkGraph};
use lightning::routing::scoring::Scorer;
use lightning::util::ser::ReadableArgs;
use lightning_background_processor::BackgroundProcessor;
use lightning_invoice::payment;
use lightning_invoice::utils::DefaultRouter;
use lightning_persister::storage::{FileSystemStorage, NodesAddressesMap};
use lightning_persister::LightningPersister;
use parking_lot::Mutex as PaMutex;
use rpc::v1::types::H256;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const CHECK_FOR_NEW_BEST_BLOCK_INTERVAL: u64 = 60;
const NETWORK_GRAPH_PERSIST_INTERVAL: u64 = 600;
const SCORER_PERSIST_INTERVAL: u64 = 600;

pub type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<PlatformFields>,
    Arc<UtxoStandardCoin>,
    Arc<PlatformFields>,
    Arc<LogState>,
    Arc<LightningPersister>,
>;

pub type ChannelManager = SimpleArcChannelManager<ChainMonitor, UtxoStandardCoin, PlatformFields, LogState>;

type Router = DefaultRouter<Arc<NetworkGraph>, Arc<LogState>>;

pub type InvoicePayer<E> = payment::InvoicePayer<Arc<ChannelManager>, Router, Arc<Mutex<Scorer>>, Arc<LogState>, E>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LightningParams {
    // The listening port for the p2p LN node
    pub listening_port: u16,
    // Printable human-readable string to describe this node to other users.
    pub node_name: [u8; 32],
    // Node's RGB color. This is used for showing the node in a network graph with the desired color.
    pub node_color: [u8; 3],
    // Invoice Payer is initialized while starting the lightning node, and it requires the number of payment retries that
    // it should do before considering a payment failed or partially failed. If not provided the number of retries will be 5
    // as this is a good default value.
    pub payment_retries: Option<usize>,
    // Node's backup path for channels and other data that requires backup.
    pub backup_path: Option<String>,
}

pub fn ln_data_dir(ctx: &MmArc, ticker: &str) -> PathBuf { ctx.dbdir().join("LIGHTNING").join(ticker) }

pub fn ln_data_backup_dir(ctx: &MmArc, path: Option<String>, ticker: &str) -> Option<PathBuf> {
    path.map(|p| {
        PathBuf::from(&p)
            .join(&hex::encode(&**ctx.rmd160()))
            .join("LIGHTNING")
            .join(ticker)
    })
}

async fn init_persister(
    ctx: &MmArc,
    ticker: String,
    backup_path: Option<String>,
) -> EnableLightningResult<Arc<LightningPersister>> {
    let ln_data_dir = ln_data_dir(ctx, &ticker);
    let ln_data_backup_dir = ln_data_backup_dir(ctx, backup_path, &ticker);
    let persister = Arc::new(LightningPersister::new(
        ln_data_dir,
        ln_data_backup_dir,
        ctx.sqlite_connection
            .ok_or(MmError::new(EnableLightningError::SqlError(
                "sqlite_connection is not initialized".into(),
            )))?
            .clone(),
    ));
    let is_initialized = persister.is_fs_initialized().await?;
    if !is_initialized {
        persister.init_fs().await?;
    }
    Ok(persister)
}

fn init_keys_manager(ctx: &MmArc) -> EnableLightningResult<Arc<KeysManager>> {
    // The current time is used to derive random numbers from the seed where required, to ensure all random generation is unique across restarts.
    let seed: [u8; 32] = ctx.secp256k1_key_pair().private().secret.into();
    let cur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_to_mm(|e| EnableLightningError::SystemTimeError(e.to_string()))?;

    Ok(Arc::new(KeysManager::new(&seed, cur.as_secs(), cur.subsec_nanos())))
}

async fn init_channel_manager(
    platform_fields: Arc<PlatformFields>,
    logger: Arc<LogState>,
    persister: Arc<LightningPersister>,
    keys_manager: Arc<KeysManager>,
    conf: LightningCoinConf,
) -> EnableLightningResult<(Arc<ChainMonitor>, Arc<ChannelManager>)> {
    // Initialize the FeeEstimator. UtxoStandardCoin implements the FeeEstimator trait, so it'll act as our fee estimator.
    let fee_estimator = platform_fields.clone();

    // Initialize the BroadcasterInterface. UtxoStandardCoin implements the BroadcasterInterface trait, so it'll act as our transaction
    // broadcaster.
    let broadcaster = Arc::new(platform_fields.platform_coin.clone());

    // Initialize the ChainMonitor
    let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
        Some(platform_fields.clone()),
        broadcaster.clone(),
        logger.clone(),
        fee_estimator.clone(),
        persister.clone(),
    ));

    // Read ChannelMonitor state from disk, important for lightning node is restarting and has at least 1 channel
    let mut channelmonitors = persister
        .read_channelmonitors(keys_manager.clone())
        .map_to_mm(|e| EnableLightningError::IOError(e.to_string()))?;

    // This is used for Electrum only to prepare for chain synchronization
    for (_, chan_mon) in channelmonitors.iter() {
        chan_mon.load_outputs_to_watch(&platform_fields);
    }

    let rpc_client = match &platform_fields.platform_coin.as_ref().rpc_client {
        UtxoRpcClientEnum::Electrum(c) => c.clone(),
        UtxoRpcClientEnum::Native(_) => {
            return MmError::err(EnableLightningError::UnsupportedMode(
                "Lightning network".into(),
                "electrum".into(),
            ))
        },
    };
    let best_header = get_best_header(&rpc_client).await?;
    let best_block = RpcBestBlock::from(best_header.clone());
    let best_block_hash = BlockHash::from_hash(
        sha256d::Hash::from_slice(&best_block.hash.0).map_to_mm(|e| EnableLightningError::HashError(e.to_string()))?,
    );
    let (channel_manager_blockhash, channel_manager) = {
        let user_config = conf.into();
        if let Ok(mut f) = File::open(persister.manager_path()) {
            let mut channel_monitor_mut_references = Vec::new();
            for (_, channel_monitor) in channelmonitors.iter_mut() {
                channel_monitor_mut_references.push(channel_monitor);
            }
            // Read ChannelManager data from the file
            let read_args = ChannelManagerReadArgs::new(
                keys_manager.clone(),
                fee_estimator.clone(),
                chain_monitor.clone(),
                broadcaster.clone(),
                logger.clone(),
                user_config,
                channel_monitor_mut_references,
            );
            <(BlockHash, ChannelManager)>::read(&mut f, read_args)
                .map_to_mm(|e| EnableLightningError::IOError(e.to_string()))?
        } else {
            // Initialize the ChannelManager to starting a new node without history
            let chain_params = ChainParameters {
                network: platform_fields.network.clone().into(),
                best_block: BestBlock::new(best_block_hash, best_block.height as u32),
            };
            let new_channel_manager = channelmanager::ChannelManager::new(
                fee_estimator.clone(),
                chain_monitor.clone(),
                broadcaster.clone(),
                logger.clone(),
                keys_manager.clone(),
                user_config,
                chain_params,
            );
            (best_block_hash, new_channel_manager)
        }
    };

    let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);

    // Sync ChannelMonitors and ChannelManager to chain tip if the node is restarting and has open channels
    if channel_manager_blockhash != best_block_hash {
        process_txs_unconfirmations(platform_fields.clone(), chain_monitor.clone(), channel_manager.clone()).await;
        process_txs_confirmations(
            // It's safe to use unwrap here for now until implementing Native Client for Lightning
            platform_fields.clone(),
            rpc_client.clone(),
            chain_monitor.clone(),
            channel_manager.clone(),
            best_header.block_height(),
        )
        .await;
        update_best_block(chain_monitor.clone(), channel_manager.clone(), best_header).await;
    }

    // Give ChannelMonitors to ChainMonitor
    for (_, channel_monitor) in channelmonitors.drain(..) {
        let funding_outpoint = channel_monitor.get_funding_txo().0;
        chain_monitor
            .watch_channel(funding_outpoint, channel_monitor)
            .map_to_mm(|e| EnableLightningError::IOError(format!("{:?}", e)))?;
    }

    // Update best block whenever there's a new chain tip or a block has been newly disconnected
    spawn(ln_best_block_update_loop(
        // It's safe to use unwrap here for now until implementing Native Client for Lightning
        platform_fields,
        chain_monitor.clone(),
        channel_manager.clone(),
        rpc_client.clone(),
        best_block,
    ));

    Ok((chain_monitor, channel_manager))
}

async fn persist_network_graph_loop(persister: Arc<LightningPersister>, network_graph: Arc<NetworkGraph>) {
    loop {
        if let Err(e) = persister.save_network_graph(network_graph.clone()).await {
            log::warn!(
                "Failed to persist network graph error: {}, please check disk space and permissions",
                e
            );
        }
        Timer::sleep(NETWORK_GRAPH_PERSIST_INTERVAL as f64).await;
    }
}

async fn persist_scorer_loop(persister: Arc<LightningPersister>, scorer: Arc<Mutex<Scorer>>) {
    loop {
        if let Err(e) = persister.save_scorer(scorer.clone()).await {
            log::warn!(
                "Failed to persist scorer error: {}, please check disk space and permissions",
                e
            );
        }
        Timer::sleep(SCORER_PERSIST_INTERVAL as f64).await;
    }
}

async fn get_open_channels_nodes_addresses(
    persister: Arc<LightningPersister>,
    channel_manager: Arc<ChannelManager>,
) -> EnableLightningResult<NodesAddressesMap> {
    let channels = channel_manager.list_channels();
    let mut nodes_addresses = persister.get_nodes_addresses().await?;
    nodes_addresses.retain(|pubkey, _node_addr| {
        channels
            .iter()
            .map(|chan| chan.counterparty.node_id)
            .any(|node_id| node_id == *pubkey)
    });
    Ok(nodes_addresses)
}

pub async fn start_lightning(
    ctx: &MmArc,
    platform_coin: UtxoStandardCoin,
    protocol_conf: LightningProtocolConf,
    conf: LightningCoinConf,
    params: LightningParams,
) -> EnableLightningResult<LightningCoin> {
    // Todo: add support for Hardware wallets for funding transactions and spending spendable outputs (channel closing transactions)
    if let DerivationMethod::HDWallet(_) = platform_coin.as_ref().derivation_method {
        return MmError::err(EnableLightningError::UnsupportedMode(
            "'start_lightning'".into(),
            "iguana".into(),
        ));
    }

    let platform_fields = Arc::new(PlatformFields::new(
        platform_coin.clone(),
        protocol_conf.network.clone(),
        protocol_conf.confirmations,
    ));

    // Initialize the Logger
    let logger = ctx.log.0.clone();

    // Initialize Persister
    let persister = init_persister(ctx, conf.ticker.clone(), params.backup_path).await?;

    // Initialize the KeysManager
    let keys_manager = init_keys_manager(ctx)?;

    // Initialize the NetGraphMsgHandler. This is used for providing routes to send payments over
    let network_graph = Arc::new(persister.get_network_graph(protocol_conf.network.into()).await?);
    spawn(persist_network_graph_loop(persister.clone(), network_graph.clone()));
    let network_gossip = Arc::new(NetGraphMsgHandler::new(
        network_graph.clone(),
        None::<Arc<dyn Access + Send + Sync>>,
        logger.clone(),
    ));

    // Initialize the ChannelManager
    let (chain_monitor, channel_manager) = init_channel_manager(
        platform_fields.clone(),
        logger.clone(),
        persister.clone(),
        keys_manager.clone(),
        conf.clone(),
    )
    .await?;

    // Initialize the PeerManager
    let peer_manager = init_peer_manager(
        ctx.clone(),
        params.listening_port,
        channel_manager.clone(),
        network_gossip.clone(),
        keys_manager.get_node_secret(),
        logger.clone(),
    )
    .await?;

    let inbound_payments = Arc::new(PaMutex::new(HashMap::new()));
    let outbound_payments = Arc::new(PaMutex::new(HashMap::new()));

    // Initialize the event handler
    let event_handler = Arc::new(ln_events::LightningEventHandler::new(
        // It's safe to use unwrap here for now until implementing Native Client for Lightning
        platform_fields.clone(),
        channel_manager.clone(),
        keys_manager.clone(),
        inbound_payments.clone(),
        outbound_payments.clone(),
    ));

    // Initialize routing Scorer
    let scorer = Arc::new(Mutex::new(persister.get_scorer().await?));
    spawn(persist_scorer_loop(persister.clone(), scorer.clone()));

    // Create InvoicePayer
    let router = DefaultRouter::new(network_graph, logger.clone());
    let invoice_payer = Arc::new(InvoicePayer::new(
        channel_manager.clone(),
        router,
        scorer,
        logger.clone(),
        event_handler,
        payment::RetryAttempts(params.payment_retries.unwrap_or(5)),
    ));

    // Persist ChannelManager
    // Note: if the ChannelManager is not persisted properly to disk, there is risk of channels force closing the next time LN starts up
    let channel_manager_persister = persister.clone();
    let persist_channel_manager_callback =
        move |node: &ChannelManager| channel_manager_persister.persist_manager(&*node);

    // Start Background Processing. Runs tasks periodically in the background to keep LN node operational.
    // InvoicePayer will act as our event handler as it handles some of the payments related events before
    // delegating it to LightningEventHandler.
    let background_processor = Arc::new(BackgroundProcessor::start(
        persist_channel_manager_callback,
        invoice_payer.clone(),
        chain_monitor.clone(),
        channel_manager.clone(),
        Some(network_gossip),
        peer_manager.clone(),
        logger,
    ));

    // If channel_nodes_data file exists, read channels nodes data from disk and reconnect to channel nodes/peers if possible.
    let open_channels_nodes = Arc::new(PaMutex::new(
        get_open_channels_nodes_addresses(persister.clone(), channel_manager.clone()).await?,
    ));
    spawn(connect_to_nodes_loop(open_channels_nodes.clone(), peer_manager.clone()));

    // Broadcast Node Announcement
    spawn(ln_node_announcement_loop(
        channel_manager.clone(),
        params.node_name,
        params.node_color,
        params.listening_port,
    ));

    Ok(LightningCoin {
        platform_fields,
        conf,
        peer_manager,
        background_processor,
        channel_manager,
        chain_monitor,
        keys_manager,
        invoice_payer,
        persister,
        inbound_payments,
        outbound_payments,
        open_channels_nodes,
    })
}

struct ConfirmedTransactionInfo {
    txid: Txid,
    header: BlockHeader,
    index: usize,
    transaction: Transaction,
    height: u32,
}

impl ConfirmedTransactionInfo {
    fn new(txid: Txid, header: BlockHeader, index: usize, transaction: Transaction, height: u32) -> Self {
        ConfirmedTransactionInfo {
            txid,
            header,
            index,
            transaction,
            height,
        }
    }
}

async fn process_tx_for_unconfirmation<T>(txid: Txid, filter: Arc<PlatformFields>, monitor: Arc<T>)
where
    T: Confirm,
{
    if let Err(err) = filter
        .platform_coin
        .as_ref()
        .rpc_client
        .get_transaction_bytes(&H256::from(txid.as_hash().into_inner()).reversed())
        .compat()
        .await
        .map_err(|e| e.into_inner())
    {
        if let UtxoRpcError::ResponseParseError(ref json_err) = err {
            if let JsonRpcErrorType::Response(_, json) = &json_err.error {
                if let Some(message) = json["message"].as_str() {
                    if message.contains("'code': -5") {
                        log::info!(
                            "Transaction {} is not found on chain :{}. The transaction will be re-broadcasted.",
                            txid,
                            err
                        );
                        monitor.transaction_unconfirmed(&txid);
                    }
                }
            }
        }
        log::error!(
            "Error while trying to check if the transaction {} is discarded or not :{}",
            txid,
            err
        );
    }
}

async fn process_txs_unconfirmations(
    filter: Arc<PlatformFields>,
    chain_monitor: Arc<ChainMonitor>,
    channel_manager: Arc<ChannelManager>,
) {
    // Retrieve channel manager transaction IDs to check the chain for un-confirmations
    let channel_manager_relevant_txids = channel_manager.get_relevant_txids();
    for txid in channel_manager_relevant_txids {
        process_tx_for_unconfirmation(txid, filter.clone(), channel_manager.clone()).await;
    }

    // Retrieve chain monitor transaction IDs to check the chain for un-confirmations
    let chain_monitor_relevant_txids = chain_monitor.get_relevant_txids();
    for txid in chain_monitor_relevant_txids {
        process_tx_for_unconfirmation(txid, filter.clone(), chain_monitor.clone()).await;
    }
}

async fn get_confirmed_registered_txs(
    filter: Arc<PlatformFields>,
    client: &ElectrumClient,
    current_height: u64,
) -> Vec<ConfirmedTransactionInfo> {
    let registered_txs = filter.registered_txs.lock().clone();
    let mut confirmed_registered_txs = Vec::new();
    for (txid, scripts) in registered_txs {
        let rpc_txid = H256::from(txid.as_hash().into_inner()).reversed();
        match filter
            .platform_coin
            .as_ref()
            .rpc_client
            .get_transaction_bytes(&rpc_txid)
            .compat()
            .await
        {
            Ok(bytes) => {
                let transaction: Transaction = match deserialize(&bytes.into_vec()) {
                    Ok(tx) => tx,
                    Err(e) => {
                        log::error!("Transaction deserialization error: {}", e.to_string());
                        continue;
                    },
                };
                for (_, vout) in transaction.output.iter().enumerate() {
                    if scripts.contains(&vout.script_pubkey) {
                        let script_hash = hex::encode(electrum_script_hash(vout.script_pubkey.as_ref()));
                        let history = client
                            .scripthash_get_history(&script_hash)
                            .compat()
                            .await
                            .unwrap_or_default();
                        for item in history {
                            if item.tx_hash == rpc_txid {
                                // If a new block mined the transaction while running process_txs_confirmations it will be confirmed later in ln_best_block_update_loop
                                if item.height > 0 && item.height <= current_height as i64 {
                                    let height: u64 = match item.height.try_into() {
                                        Ok(h) => h,
                                        Err(e) => {
                                            log::error!("Block height convertion to u64 error: {}", e.to_string());
                                            continue;
                                        },
                                    };
                                    let header = match client.blockchain_block_header(height).compat().await {
                                        Ok(block_header) => match deserialize(&block_header) {
                                            Ok(h) => h,
                                            Err(e) => {
                                                log::error!("Block header deserialization error: {}", e.to_string());
                                                continue;
                                            },
                                        },
                                        Err(_) => continue,
                                    };
                                    let index = match client
                                        .blockchain_transaction_get_merkle(rpc_txid, height)
                                        .compat()
                                        .await
                                    {
                                        Ok(merkle_branch) => merkle_branch.pos,
                                        Err(e) => {
                                            log::error!(
                                                "Error getting transaction position in the block: {}",
                                                e.to_string()
                                            );
                                            continue;
                                        },
                                    };
                                    let confirmed_transaction_info = ConfirmedTransactionInfo::new(
                                        txid,
                                        header,
                                        index,
                                        transaction.clone(),
                                        height as u32,
                                    );
                                    confirmed_registered_txs.push(confirmed_transaction_info);
                                    filter.registered_txs.lock().remove(&txid);
                                }
                            }
                        }
                    }
                }
            },
            Err(e) => {
                log::error!("Error getting transaction {} from chain: {}", txid, e);
                continue;
            },
        };
    }
    confirmed_registered_txs
}

async fn append_spent_registered_output_txs(
    transactions_to_confirm: &mut Vec<ConfirmedTransactionInfo>,
    filter: Arc<PlatformFields>,
    client: &ElectrumClient,
) {
    let mut outputs_to_remove = Vec::new();
    let registered_outputs = filter.registered_outputs.lock().clone();
    for output in registered_outputs {
        let result = match ln_rpc::find_watched_output_spend_with_header(client, &output).await {
            Ok(res) => res,
            Err(e) => {
                log::error!(
                    "Error while trying to find if the registered output {:?} is spent: {}",
                    output.outpoint,
                    e
                );
                continue;
            },
        };
        if let Some((header, _, tx, height)) = result {
            if !transactions_to_confirm.iter().any(|info| info.txid == tx.txid()) {
                let rpc_txid = H256::from(tx.txid().as_hash().into_inner()).reversed();
                let index = match client
                    .blockchain_transaction_get_merkle(rpc_txid, height)
                    .compat()
                    .await
                {
                    Ok(merkle_branch) => merkle_branch.pos,
                    Err(e) => {
                        log::error!("Error getting transaction position in the block: {}", e.to_string());
                        continue;
                    },
                };
                let confirmed_transaction_info =
                    ConfirmedTransactionInfo::new(tx.txid(), header, index, tx, height as u32);
                transactions_to_confirm.push(confirmed_transaction_info);
            }
            outputs_to_remove.push(output);
        }
    }
    filter
        .registered_outputs
        .lock()
        .retain(|output| !outputs_to_remove.contains(output));
}

async fn process_txs_confirmations(
    filter: Arc<PlatformFields>,
    client: ElectrumClient,
    chain_monitor: Arc<ChainMonitor>,
    channel_manager: Arc<ChannelManager>,
    current_height: u64,
) {
    let mut transactions_to_confirm = get_confirmed_registered_txs(filter.clone(), &client, current_height).await;
    append_spent_registered_output_txs(&mut transactions_to_confirm, filter.clone(), &client).await;

    transactions_to_confirm.sort_by(|a, b| {
        let block_order = a.height.cmp(&b.height);
        match block_order {
            Ordering::Equal => a.index.cmp(&b.index),
            _ => block_order,
        }
    });

    for confirmed_transaction_info in transactions_to_confirm {
        channel_manager.transactions_confirmed(
            &confirmed_transaction_info.header,
            &[(
                confirmed_transaction_info.index,
                &confirmed_transaction_info.transaction,
            )],
            confirmed_transaction_info.height,
        );
        chain_monitor.transactions_confirmed(
            &confirmed_transaction_info.header,
            &[(
                confirmed_transaction_info.index,
                &confirmed_transaction_info.transaction,
            )],
            confirmed_transaction_info.height,
        );
    }
}

async fn get_best_header(best_header_listener: &ElectrumClient) -> EnableLightningResult<ElectrumBlockHeader> {
    best_header_listener
        .blockchain_headers_subscribe()
        .compat()
        .await
        .map_to_mm(|e| EnableLightningError::RpcError(e.to_string()))
}

async fn update_best_block(
    chain_monitor: Arc<ChainMonitor>,
    channel_manager: Arc<ChannelManager>,
    best_header: ElectrumBlockHeader,
) {
    {
        let (new_best_header, new_best_height) = match best_header {
            ElectrumBlockHeader::V12(h) => {
                let nonce = match h.nonce {
                    ElectrumNonce::Number(n) => n as u32,
                    ElectrumNonce::Hash(_) => {
                        return;
                    },
                };
                let prev_blockhash = match sha256d::Hash::from_slice(&h.prev_block_hash.0) {
                    Ok(h) => h,
                    Err(e) => {
                        log::error!("Error while parsing previous block hash for lightning node: {}", e);
                        return;
                    },
                };
                let merkle_root = match sha256d::Hash::from_slice(&h.merkle_root.0) {
                    Ok(h) => h,
                    Err(e) => {
                        log::error!("Error while parsing merkle root for lightning node: {}", e);
                        return;
                    },
                };
                (
                    BlockHeader {
                        version: h.version as i32,
                        prev_blockhash: BlockHash::from_hash(prev_blockhash),
                        merkle_root: TxMerkleNode::from_hash(merkle_root),
                        time: h.timestamp as u32,
                        bits: h.bits as u32,
                        nonce,
                    },
                    h.block_height as u32,
                )
            },
            ElectrumBlockHeader::V14(h) => {
                let block_header = match deserialize(&h.hex.into_vec()) {
                    Ok(header) => header,
                    Err(e) => {
                        log::error!("Block header deserialization error: {}", e.to_string());
                        return;
                    },
                };
                (block_header, h.height as u32)
            },
        };
        channel_manager.best_block_updated(&new_best_header, new_best_height);
        chain_monitor.best_block_updated(&new_best_header, new_best_height);
    }
}

async fn ln_best_block_update_loop(
    filter: Arc<PlatformFields>,
    chain_monitor: Arc<ChainMonitor>,
    channel_manager: Arc<ChannelManager>,
    best_header_listener: ElectrumClient,
    best_block: RpcBestBlock,
) {
    let mut current_best_block = best_block;
    loop {
        let best_header = match get_best_header(&best_header_listener).await {
            Ok(h) => h,
            Err(e) => {
                log::error!("Error while requesting best header for lightning node: {}", e);
                Timer::sleep(CHECK_FOR_NEW_BEST_BLOCK_INTERVAL as f64).await;
                continue;
            },
        };
        if current_best_block != best_header.clone().into() {
            process_txs_unconfirmations(filter.clone(), chain_monitor.clone(), channel_manager.clone()).await;
            process_txs_confirmations(
                filter.clone(),
                best_header_listener.clone(),
                chain_monitor.clone(),
                channel_manager.clone(),
                best_header.block_height(),
            )
            .await;
            current_best_block = best_header.clone().into();
            update_best_block(chain_monitor.clone(), channel_manager.clone(), best_header).await;
        }
        Timer::sleep(CHECK_FOR_NEW_BEST_BLOCK_INTERVAL as f64).await;
    }
}
