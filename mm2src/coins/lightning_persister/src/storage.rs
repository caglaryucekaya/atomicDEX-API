use async_trait::async_trait;
use bitcoin::Network;
use lightning::routing::network_graph::NetworkGraph;
use lightning::routing::scoring::Scorer;
use parking_lot::Mutex as PaMutex;
use secp256k1::PublicKey;
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

pub type NodesAddressesMap = HashMap<PublicKey, SocketAddr>;
pub type NodesAddressesMapShared = Arc<PaMutex<NodesAddressesMap>>;

#[async_trait]
pub trait FileSystemStorage {
    type Error;

    /// Initializes dirs/collection/tables in storage for a specified coin
    async fn init_fs(&self) -> Result<(), Self::Error>;

    async fn is_fs_initialized(&self) -> Result<bool, Self::Error>;

    async fn get_nodes_addresses(&self) -> Result<HashMap<PublicKey, SocketAddr>, Self::Error>;

    async fn save_nodes_addresses(&self, nodes_addresses: NodesAddressesMapShared) -> Result<(), Self::Error>;

    async fn get_network_graph(&self, network: Network) -> Result<NetworkGraph, Self::Error>;

    async fn save_network_graph(&self, network_graph: Arc<NetworkGraph>) -> Result<(), Self::Error>;

    async fn get_scorer(&self) -> Result<Scorer, Self::Error>;

    async fn save_scorer(&self, scorer: Arc<Mutex<Scorer>>) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SqlChannelDetails {
    pub rpc_id: u64,
    pub channel_id: String,
    pub counterparty_node_id: String,
    pub funding_tx: String,
    pub initial_balance: u64,
    pub closing_tx: String,
    pub closing_balance: u64,
    pub closure_reason: String,
    pub is_outbound: bool,
    pub is_public: bool,
    pub is_closed: bool,
}

impl SqlChannelDetails {
    pub fn new(
        rpc_id: u64,
        channel_id: [u8; 32],
        counterparty_node_id: PublicKey,
        initial_balance: u64,
        is_outbound: bool,
        is_public: bool,
    ) -> Self {
        SqlChannelDetails {
            rpc_id,
            channel_id: hex::encode(channel_id),
            counterparty_node_id: counterparty_node_id.to_string(),
            funding_tx: "".into(),
            initial_balance,
            closing_tx: "".into(),
            closing_balance: 0,
            closure_reason: "".into(),
            is_outbound,
            is_public,
            is_closed: false,
        }
    }
}

#[async_trait]
pub trait SqlStorage {
    type Error;

    /// Initializes dirs/collection/tables in storage for a specified coin
    async fn init_sql(&self) -> Result<(), Self::Error>;

    async fn is_sql_initialized(&self) -> Result<bool, Self::Error>;

    async fn add_channel_to_sql(&self, details: SqlChannelDetails) -> Result<(), Self::Error>;

    async fn get_channel_from_sql(&self, rpc_id: u64) -> Result<Option<SqlChannelDetails>, Self::Error>;

    async fn get_last_channel_rpc_id(&self) -> Result<u32, Self::Error>;

    async fn add_funding_tx_to_sql(
        &self,
        rpc_id: u64,
        funding_tx: String,
        initial_balance: u64,
    ) -> Result<(), Self::Error>;

    async fn update_channel_to_closed(&self, rpc_id: u64, closure_reason: String) -> Result<(), Self::Error>;

    async fn get_closed_channels(&self) -> Result<Vec<SqlChannelDetails>, Self::Error>;
}
