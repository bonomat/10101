use crate::bitcoin_conversion::to_network_29;
use crate::bitcoin_conversion::to_secp_pk_30;
use crate::blockchain::Blockchain;
use crate::dlc_custom_signer::CustomKeysManager;
use crate::dlc_wallet::DlcWallet;
use crate::fee_rate_estimator::FeeRateEstimator;
use crate::ln::TracingLogger;
use crate::message_handler::TenTenOneMessageHandler;
use crate::node::event::connect_node_event_handler_to_dlc_channel_events;
use crate::node::event::NodeEventHandler;
use crate::on_chain_wallet::BdkStorage;
use crate::on_chain_wallet::OnChainWallet;
use crate::seed::Bip39Seed;
use crate::shadow::Shadow;
use crate::storage::DlcChannelEvent;
use crate::storage::DlcStorageProvider;
use crate::storage::TenTenOneStorage;
use crate::ChainMonitor;
use crate::NetworkGraph;
use crate::P2pGossipSync;
use crate::PeerManager;
use anyhow::Result;
use bdk::FeeRate;
use bitcoin::address::NetworkUnchecked;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::XOnlyPublicKey;
use bitcoin::Address;
use bitcoin::Network;
use bitcoin::Txid;
use futures::future::RemoteHandle;
use futures::FutureExt;
use lightning::chain::chaininterface::ConfirmationTarget;
use lightning::chain::chainmonitor;
use lightning::ln::peer_handler::MessageHandler;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScorer;
use lightning::routing::scoring::ProbabilisticScoringDecayParameters;
use lightning::routing::scoring::ProbabilisticScoringFeeParameters;
use lightning::routing::utxo::UtxoLookup;
use lightning::sign::EntropySource;
use lightning::sign::KeysManager;
use lightning::util::config::UserConfig;
use lightning_transaction_sync::EsploraSyncClient;
use p2pd_oracle_client::P2PDOracleClient;
use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;
use serde_with::DurationSeconds;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::sync::RwLock;
use tokio::task::spawn_blocking;

mod channel_manager;
mod connection;
mod dlc_manager;
mod oracle;
mod storage;
mod sub_channel_manager;
mod wallet;

pub mod dlc_channel;
pub mod event;
pub mod peer_manager;

pub use crate::message_handler::tentenone_message_name;
pub use ::dlc_manager as rust_dlc_manager;
pub use channel_manager::ChannelManager;
pub use connection::TenTenOneOnionMessageHandler;
pub use dlc_manager::signed_channel_state_name;
pub use dlc_manager::DlcManager;
pub use oracle::OracleInfo;
pub use storage::InMemoryStore;
pub use storage::Storage;
pub use sub_channel_manager::SubChannelManager;

/// A node.
pub struct Node<D: BdkStorage, S: TenTenOneStorage, N: Storage> {
    pub settings: Arc<RwLock<XXINodeSettings>>,
    pub network: Network,

    pub(crate) wallet: Arc<OnChainWallet<D>>,
    pub blockchain: Arc<Blockchain<N>>,

    // Making this public is only necessary because of the collaborative revert protocol.
    pub dlc_wallet: Arc<DlcWallet<D, S, N>>,

    pub peer_manager: Arc<PeerManager<D, S, N>>,
    pub channel_manager: Arc<ChannelManager<D, S, N>>,
    pub chain_monitor: Arc<ChainMonitor<S, N>>,
    pub keys_manager: Arc<CustomKeysManager<D>>,
    pub network_graph: Arc<NetworkGraph>,
    pub fee_rate_estimator: Arc<FeeRateEstimator>,

    pub logger: Arc<TracingLogger>,

    pub info: NodeInfo,

    pub dlc_manager: Arc<DlcManager<D, S, N>>,
    pub sub_channel_manager: Arc<SubChannelManager<D, S, N>>,

    /// All oracles clients the node is aware of.
    pub oracles: Vec<Arc<P2PDOracleClient>>,
    pub dlc_message_handler: Arc<TenTenOneMessageHandler>,
    pub ldk_config: Arc<parking_lot::RwLock<UserConfig>>,

    /// The oracle pubkey used for proposing dlc channels
    pub oracle_pubkey: XOnlyPublicKey,

    pub event_handler: Arc<NodeEventHandler>,

    // storage
    // TODO(holzeis): The node storage should get extracted to the corresponding application
    // layers.
    pub node_storage: Arc<N>,
    pub ln_storage: Arc<S>,
    pub dlc_storage: Arc<DlcStorageProvider<S>>,

    // fields below are needed only to start the node
    #[allow(dead_code)]
    listen_address: SocketAddr, // Irrelevant when using websockets
}

/// An on-chain network fee for a transaction
pub enum Fee {
    /// A fee given by the transaction's priority
    Priority(ConfirmationTarget),
    /// A fix defined sats/vbyte
    FeeRate(FeeRate),
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct NodeInfo {
    pub pubkey: PublicKey,
    pub address: SocketAddr,
    pub is_ws: bool,
}

/// Node is running until this struct is dropped
pub struct RunningNode {
    _handles: Vec<RemoteHandle<()>>,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct XXINodeSettings {
    /// How often we sync the off chain wallet
    #[serde_as(as = "DurationSeconds")]
    pub off_chain_sync_interval: Duration,
    /// How often we sync the BDK wallet
    #[serde_as(as = "DurationSeconds")]
    pub on_chain_sync_interval: Duration,
    /// How often we update the fee rate
    #[serde_as(as = "DurationSeconds")]
    pub fee_rate_sync_interval: Duration,
    /// How often we run the [`SubChannelManager`]'s periodic check.
    #[serde_as(as = "DurationSeconds")]
    pub sub_channel_manager_periodic_check_interval: Duration,
    /// How often we sync the shadow states
    #[serde_as(as = "DurationSeconds")]
    pub shadow_sync_interval: Duration,
}

impl<D: BdkStorage, S: TenTenOneStorage + 'static, N: Storage + Sync + Send + 'static>
    Node<D, S, N>
{
    pub async fn update_settings(&self, new_settings: XXINodeSettings) {
        tracing::info!(?new_settings, "Updating LnDlcNode settings");
        *self.settings.write().await = new_settings;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        // Supplied configuration of LDK node.
        ldk_config: UserConfig,
        alias: &str,
        network: Network,
        data_dir: &Path,
        storage: S,
        node_storage: Arc<N>,
        wallet_storage: D,
        announcement_address: SocketAddr,
        listen_address: SocketAddr,
        electrs_server_url: String,
        seed: Bip39Seed,
        ephemeral_randomness: [u8; 32],
        settings: XXINodeSettings,
        oracle_clients: Vec<P2PDOracleClient>,
        oracle_pubkey: XOnlyPublicKey,
        node_event_handler: Arc<NodeEventHandler>,
        dlc_event_sender: mpsc::Sender<DlcChannelEvent>,
    ) -> Result<Self> {
        let time_since_unix_epoch = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;

        let logger = Arc::new(TracingLogger {
            alias: alias.to_string(),
        });

        let ldk_config = Arc::new(parking_lot::RwLock::new(ldk_config));

        let fee_rate_estimator = Arc::new(FeeRateEstimator::new(network));

        let on_chain_wallet = OnChainWallet::new(
            network,
            seed.wallet_seed(),
            wallet_storage,
            fee_rate_estimator.clone(),
        )?;
        let on_chain_wallet = Arc::new(on_chain_wallet);

        let blockchain = Blockchain::new(electrs_server_url.clone(), node_storage.clone())?;
        let blockchain = Arc::new(blockchain);

        let esplora_client = Arc::new(EsploraSyncClient::new(
            electrs_server_url.clone(),
            logger.clone(),
        ));

        let dlc_storage = Arc::new(DlcStorageProvider::new(storage.clone(), dlc_event_sender));
        let ln_storage = Arc::new(storage);

        let chain_monitor: Arc<ChainMonitor<S, N>> = Arc::new(chainmonitor::ChainMonitor::new(
            Some(esplora_client.clone()),
            blockchain.clone(),
            logger.clone(),
            fee_rate_estimator.clone(),
            ln_storage.clone(),
        ));

        let keys_manager = {
            Arc::new(CustomKeysManager::new(
                KeysManager::new(
                    &seed.lightning_seed(),
                    time_since_unix_epoch.as_secs(),
                    time_since_unix_epoch.subsec_nanos(),
                ),
                on_chain_wallet.clone(),
            ))
        };

        let network_graph = Arc::new(NetworkGraph::new(to_network_29(network), logger.clone()));

        let scorer = ProbabilisticScorer::new(
            ProbabilisticScoringDecayParameters::default(),
            network_graph.clone(),
            logger.clone(),
        );
        let scorer = std::sync::RwLock::new(scorer);
        let scorer = Arc::new(scorer);

        let scoring_fee_params = ProbabilisticScoringFeeParameters::default();
        let router = Arc::new(DefaultRouter::new(
            network_graph.clone(),
            logger.clone(),
            keys_manager.get_secure_random_bytes(),
            scorer.clone(),
            scoring_fee_params,
        ));

        let channel_manager = channel_manager::build(
            keys_manager.clone(),
            blockchain.clone(),
            fee_rate_estimator.clone(),
            esplora_client.clone(),
            logger.clone(),
            chain_monitor.clone(),
            *ldk_config.read(),
            network,
            ln_storage.clone(),
            router,
        )?;

        let channel_manager = Arc::new(channel_manager);

        let gossip_sync = Arc::new(P2pGossipSync::new(
            network_graph.clone(),
            None::<Arc<dyn UtxoLookup + Send + Sync>>,
            logger.clone(),
        ));

        let oracle_clients: Vec<Arc<P2PDOracleClient>> =
            oracle_clients.into_iter().map(Arc::new).collect();

        let dlc_wallet = DlcWallet::new(
            on_chain_wallet.clone(),
            dlc_storage.clone(),
            blockchain.clone(),
        );
        let dlc_wallet = Arc::new(dlc_wallet);

        let dlc_manager = dlc_manager::build(
            data_dir,
            dlc_wallet.clone(),
            dlc_storage.clone(),
            oracle_clients.clone(),
            fee_rate_estimator.clone(),
        )?;
        let dlc_manager = Arc::new(dlc_manager);

        let sub_channel_manager = sub_channel_manager::build(
            channel_manager.clone(),
            dlc_manager.clone(),
            chain_monitor.clone(),
            keys_manager.clone(),
        )?;

        let dlc_message_handler = Arc::new(TenTenOneMessageHandler::new());

        let onion_message_handler = Arc::new(TenTenOneOnionMessageHandler::new(
            node_event_handler.clone(),
        ));

        let lightning_msg_handler = MessageHandler {
            chan_handler: sub_channel_manager.clone(),
            route_handler: gossip_sync.clone(),
            onion_message_handler,
            custom_message_handler: dlc_message_handler.clone(),
        };

        let peer_manager: Arc<PeerManager<D, S, N>> = Arc::new(PeerManager::new(
            lightning_msg_handler,
            time_since_unix_epoch.as_secs() as u32,
            &ephemeral_randomness,
            logger.clone(),
            keys_manager.clone(),
        ));

        let node_info = NodeInfo {
            pubkey: to_secp_pk_30(channel_manager.get_our_node_id()),
            address: announcement_address,
            is_ws: false,
        };

        let settings = Arc::new(RwLock::new(settings));

        Ok(Self {
            network,
            wallet: on_chain_wallet,
            blockchain,
            dlc_wallet,
            peer_manager,
            keys_manager,
            chain_monitor,
            logger,
            channel_manager: channel_manager.clone(),
            info: node_info,
            sub_channel_manager,
            oracles: oracle_clients,
            dlc_message_handler,
            dlc_manager,
            ln_storage,
            dlc_storage,
            node_storage,
            fee_rate_estimator,
            ldk_config,
            network_graph,
            settings,
            listen_address,
            oracle_pubkey,
            event_handler: node_event_handler,
        })
    }

    /// Starts the background handles - if the returned handles are dropped, the
    /// background tasks are stopped.
    // TODO: Consider having handles for *all* the tasks & threads for a clean shutdown.
    pub fn start(
        &self,
        dlc_event_receiver: mpsc::Receiver<DlcChannelEvent>,
    ) -> Result<RunningNode> {
        #[cfg(feature = "ln_net_tcp")]
        let handles = vec![spawn_connection_management(
            self.peer_manager.clone(),
            self.listen_address,
        )];

        #[cfg(not(feature = "ln_net_tcp"))]
        let mut handles = Vec::new();

        std::thread::spawn(shadow_sync_periodically(
            self.settings.clone(),
            self.node_storage.clone(),
            self.wallet.clone(),
        ));

        tokio::spawn(update_fee_rate_estimates(
            self.settings.clone(),
            self.fee_rate_estimator.clone(),
        ));

        connect_node_event_handler_to_dlc_channel_events(
            self.event_handler.clone(),
            dlc_event_receiver,
        );

        tracing::info!("Node started with node ID {}", self.info);

        Ok(RunningNode { _handles: handles })
    }

    /// Send the given `amount_sats` sats to the given unchecked, on-chain `address`.
    pub async fn send_to_address(
        &self,
        address: Address<NetworkUnchecked>,
        amount_sats: u64,
        fee: Fee,
    ) -> Result<Txid> {
        let address = address.require_network(self.network)?;

        let tx = spawn_blocking({
            let wallet = self.wallet.clone();
            move || {
                let tx = wallet.build_on_chain_payment_tx(&address, amount_sats, fee)?;

                anyhow::Ok(tx)
            }
        })
        .await
        .expect("task to complete")?;

        let txid = self.blockchain.broadcast_transaction_blocking(&tx)?;

        Ok(txid)
    }

    pub fn list_peers(&self) -> Vec<PublicKey> {
        self.peer_manager
            .get_peer_node_ids()
            .into_iter()
            .map(|(peer, _)| to_secp_pk_30(peer))
            .collect()
    }
}

async fn update_fee_rate_estimates(
    settings: Arc<RwLock<XXINodeSettings>>,
    fee_rate_estimator: Arc<FeeRateEstimator>,
) {
    loop {
        if let Err(err) = fee_rate_estimator.update().await {
            tracing::error!("Failed to update fee rate estimates: {err:#}");
        }

        let interval = {
            let guard = settings.read().await;
            guard.fee_rate_sync_interval
        };
        tokio::time::sleep(interval).await;
    }
}

fn shadow_sync_periodically<D: BdkStorage, N: Storage>(
    settings: Arc<RwLock<XXINodeSettings>>,
    node_storage: Arc<N>,
    wallet: Arc<OnChainWallet<D>>,
) -> impl Fn() {
    let handle = tokio::runtime::Handle::current();
    let shadow = Shadow::new(node_storage, wallet);
    move || loop {
        if let Err(e) = shadow.sync_transactions() {
            tracing::error!("Failed to sync transaction shadows. Error: {e:#}");
        }

        let interval = handle.block_on(async {
            let guard = settings.read().await;
            guard.shadow_sync_interval
        });

        std::thread::sleep(interval);
    }
}

#[cfg(feature = "ln_net_tcp")]
fn spawn_connection_management<
    D: BdkStorage,
    S: TenTenOneStorage + 'static,
    N: Storage + Send + Sync + 'static,
>(
    peer_manager: Arc<PeerManager<D, S, N>>,
    listen_address: SocketAddr,
) -> RemoteHandle<()> {
    let (fut, remote_handle) = async move {
        let mut connection_handles = Vec::new();

        let listener = tokio::net::TcpListener::bind(listen_address)
            .await
            .expect("Failed to bind to listen port");
        loop {
            let peer_manager = peer_manager.clone();
            let (tcp_stream, addr) = match listener.accept().await {
                Ok(ret) => ret,
                Err(e) => {
                    tracing::error!("Failed to accept incoming connection: {e:#}");
                    continue;
                }
            };

            tracing::debug!(%addr, "Received inbound connection");

            let (fut, connection_handle) = async move {
                crate::networking::tcp::setup_inbound(
                    peer_manager.clone(),
                    tcp_stream.into_std().expect("Stream conversion to succeed"),
                )
                .await;
            }
            .remote_handle();

            connection_handles.push(connection_handle);

            tokio::spawn(fut);
        }
    }
    .remote_handle();

    tokio::spawn(fut);

    tracing::info!("Listening on {listen_address}");

    remote_handle
}

impl Display for NodeInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let scheme = if self.is_ws { "ws" } else { "tcp" };

        format!("{scheme}://{}@{}", self.pubkey, self.address).fmt(f)
    }
}
