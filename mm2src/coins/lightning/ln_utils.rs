use super::*;
use crate::lightning::ln_conf::{LightningCoinConf, LightningProtocolConf};
use crate::lightning::ln_p2p::{connect_to_nodes_loop, init_peer_manager, ln_node_announcement_loop};
use crate::lightning::ln_platform::{get_best_header, ln_best_block_update_loop, update_best_block};
use crate::utxo::rpc_clients::BestBlock as RpcBestBlock;
use crate::utxo::utxo_standard::UtxoStandardCoin;
use crate::DerivationMethod;
use bitcoin::hash_types::BlockHash;
use bitcoin_hashes::{sha256d, Hash};
use common::executor::{spawn, Timer};
use common::log;
use common::log::LogState;
use common::mm_ctx::MmArc;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager};
use lightning::chain::{chainmonitor, Access, BestBlock, Watch};
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
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const NETWORK_GRAPH_PERSIST_INTERVAL: u64 = 600;
const SCORER_PERSIST_INTERVAL: u64 = 600;

pub type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<Platform>,
    Arc<Platform>,
    Arc<Platform>,
    Arc<LogState>,
    Arc<LightningPersister>,
>;

pub type ChannelManager = SimpleArcChannelManager<ChainMonitor, Platform, Platform, LogState>;

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
    platform: Arc<Platform>,
    logger: Arc<LogState>,
    persister: Arc<LightningPersister>,
    keys_manager: Arc<KeysManager>,
    conf: LightningCoinConf,
) -> EnableLightningResult<(Arc<ChainMonitor>, Arc<ChannelManager>)> {
    // Initialize the FeeEstimator. UtxoStandardCoin implements the FeeEstimator trait, so it'll act as our fee estimator.
    let fee_estimator = platform.clone();

    // Initialize the BroadcasterInterface. UtxoStandardCoin implements the BroadcasterInterface trait, so it'll act as our transaction
    // broadcaster.
    let broadcaster = platform.clone();

    // Initialize the ChainMonitor
    let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
        Some(platform.clone()),
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
        chan_mon.load_outputs_to_watch(&platform);
    }

    let rpc_client = match &platform.coin.as_ref().rpc_client {
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
                network: platform.network.clone().into(),
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
        platform
            .process_txs_unconfirmations(chain_monitor.clone(), channel_manager.clone())
            .await;
        platform
            .process_txs_confirmations(
                // It's safe to use unwrap here for now until implementing Native Client for Lightning
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
        platform,
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

    let platform = Arc::new(Platform::new(
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
        platform.clone(),
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
        platform.clone(),
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
        platform,
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
