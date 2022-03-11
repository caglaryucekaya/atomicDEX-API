use async_trait::async_trait;
use bitcoin::Network;
use lightning::ln::channelmanager::ChannelDetails;
use lightning::routing::network_graph::NetworkGraph;
use lightning::routing::scoring::Scorer;
use parking_lot::Mutex as PaMutex;
use secp256k1::PublicKey;
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

#[async_trait]
pub trait SqlStorage {
    type Error;

    /// Initializes dirs/collection/tables in storage for a specified coin
    async fn init_sql(&self, for_coin: &str) -> Result<(), Self::Error>;

    async fn is_sql_initialized(&self, for_coin: &str) -> Result<bool, Self::Error>;

    async fn add_channel_to_history(&self, for_coin: &str, channel_details: ChannelDetails) -> Result<(), Self::Error>;

    async fn get_number_of_channels_in_sql(&self, for_coin: &str) -> Result<u32, Self::Error>;
}
