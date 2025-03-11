use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use base64::{prelude::BASE64_URL_SAFE, Engine};
use insanity_core::user_input_event::UserInputEvent;
use insanity_tui_adapter::AppEvent;

use sha2::{Digest, Sha256};
use veq::{snow_types::SnowKeypair, veq::VeqSocket};

use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::managed_peer::{ConnectionStatus, ManagedPeer};
use veq::snow_types::SnowPublicKey;

use baybridge::{
    client::Actions,
    connectors::{connection::Connection, http::HttpConnection},
};

use crate::room_handler;

const DB_KEY_PRIVATE_KEY: &str = "private_key";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AugmentedInfo {
    pub connection_info: veq::veq::ConnectionInfo,
    pub display_name: String,
}

pub struct ConnectionManager {
    socket: VeqSocket,
    cancellation_token: CancellationToken,
    user_action_tx: mpsc::UnboundedSender<UserInputEvent>,
}

impl ConnectionManager {
    pub fn builder(
        base_dir: PathBuf,
        listen_port: u16,
        bridge_servers: Vec<String>,
        ip_version: IpVersion,
    ) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder::new(base_dir, listen_port, bridge_servers, ip_version)
    }

    pub fn shutdown(&self) {
        self.cancellation_token.cancel();
    }

    pub fn send_user_action(&self, action: UserInputEvent) -> anyhow::Result<()> {
        self.user_action_tx.send(action)?;
        Ok(())
    }

    async fn start(
        &mut self,
        bridge_servers: Vec<String>,
        room_name: Option<String>,
        base_dir: PathBuf,
        display_name: Option<String>,
        app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
        user_action_rx: mpsc::UnboundedReceiver<UserInputEvent>,
    ) -> anyhow::Result<()> {
        let connection_info = self.socket.connection_info();
        log::debug!("Connection info: {:?}", connection_info);

        let conn_info_tx = manage_peers(
            self.socket.clone(),
            app_event_tx.clone(),
            user_action_rx,
            self.cancellation_token.clone(),
        );

        if let Some(room_name) = &room_name {
            log::debug!("Attempting to join room {room_name} on server {bridge_servers:?}.");

            // Start up baybridge connection.
            let baybridge_datadir = base_dir.join("baybridge");
            let bridge_server_urls = bridge_servers
                .iter()
                .map(|s| url::Url::parse(s))
                .collect::<Result<Vec<_>, _>>()?;
            let connections = bridge_server_urls
                .into_iter()
                .map(|url| Connection::Http(HttpConnection::new(url)))
                .collect();
            let baybridge_config = baybridge::configuration::Configuration::new(
                baybridge_datadir.clone(),
                connections,
            );
            baybridge_config.init().await?;
            let action = Actions::new(baybridge_config);

            // Query self and add to UI.
            if let Some(app_event_tx) = app_event_tx.clone() {
                let my_public_key = action.whoami().await;
                let my_public_key_base64 = BASE64_URL_SAFE.encode(my_public_key.as_bytes());
                if let Err(e) = app_event_tx.send(AppEvent::SetOwnPublicKey(my_public_key_base64)) {
                    log::debug!("Failed to write own public key to UI: {e}");
                }
            }

            // Start connection to room on baybridge.
            room_handler::start_room_connection(
                action,
                room_name,
                connection_info,
                display_name,
                conn_info_tx,
                app_event_tx.clone(),
                self.cancellation_token.clone(),
            )
            .await?;
        } else {
            log::debug!("Not joining any room.");
        }

        Ok(())
    }
}

#[derive(clap::ValueEnum, Clone, Debug, serde::Deserialize)]
pub enum IpVersion {
    Ipv4,
    Ipv6,
    Dualstack,
}

pub struct ConnectionManagerBuilder {
    base_dir: PathBuf,
    listen_port: u16,
    bridge_servers: Vec<String>,
    ip_version: IpVersion,
    room_name: Option<String>,
    display_name: Option<String>,
    cancellation_token: Option<CancellationToken>,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
}

impl ConnectionManagerBuilder {
    pub fn new(
        base_dir: PathBuf,
        listen_port: u16,
        bridge_servers: Vec<String>,
        ip_version: IpVersion,
    ) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            base_dir,
            listen_port,
            bridge_servers,
            ip_version,
            room_name: None,
            display_name: None,
            cancellation_token: None,
            app_event_sender: None,
        }
    }

    pub fn room(self, room_name: String) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            room_name: Some(room_name),
            ..self
        }
    }

    pub fn display_name(self, display_name: String) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            display_name: Some(display_name),
            ..self
        }
    }

    pub fn cancellation_token(
        self,
        cancellation_token: CancellationToken,
    ) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            cancellation_token: Some(cancellation_token),
            ..self
        }
    }

    pub fn app_event_sender(
        self,
        app_event_sender: mpsc::UnboundedSender<AppEvent>,
    ) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            app_event_sender: Some(app_event_sender),
            ..self
        }
    }

    /// Creates the local socket, uploads connection info, and begins searching for connections.
    pub async fn start(self) -> anyhow::Result<ConnectionManager> {
        let cancellation_token = self.cancellation_token.unwrap_or_default();

        // Create or open connection manager database.
        let sled_path = self.base_dir.join("connection_manager_data.sled");
        let db = sled::open(sled_path)?;

        // Create local socket.
        let keypair: SnowKeypair = get_or_make_keypair(&db)?;

        let socket = match self.ip_version {
            IpVersion::Ipv4 => {
                let v4_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, self.listen_port);
                VeqSocket::bind_with_keypair(&v4_addr.to_string(), keypair).await?
            }
            IpVersion::Ipv6 => {
                let v6_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, self.listen_port, 0, 0);
                VeqSocket::bind_with_keypair(&v6_addr.to_string(), keypair).await?
            }
            IpVersion::Dualstack => {
                let v4_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, self.listen_port);
                // TODO: maybe have a better way for specifying both ipv4 and ipv6 listen ports simultaneously.
                let ipv6_listen_port = if self.listen_port == 0 {
                    0
                } else {
                    self.listen_port + 1
                };
                let v6_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, ipv6_listen_port, 0, 0);
                VeqSocket::dualstack_with_keypair(v4_addr, v6_addr, keypair).await?
            }
        };

        let (user_action_tx, user_action_rx) = mpsc::unbounded_channel();
        let mut connection_manager = ConnectionManager {
            socket,
            cancellation_token,
            user_action_tx,
        };
        connection_manager
            .start(
                self.bridge_servers,
                self.room_name,
                self.base_dir,
                self.display_name,
                self.app_event_sender,
                user_action_rx,
            )
            .await?;
        Ok(connection_manager)
    }
}

/// Receive peer augmented info over channel and connect to peer.
fn manage_peers(
    socket: veq::veq::VeqSocket,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    mut user_action_rx: mpsc::UnboundedReceiver<UserInputEvent>,
    cancellation_token: CancellationToken,
) -> mpsc::UnboundedSender<AugmentedInfo> {
    // Channel for the manage_peers task to receive updated peers info.
    let (conn_info_tx, mut conn_info_rx) = mpsc::unbounded_channel::<AugmentedInfo>();
    let sender_is_muted = Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        let mut managed_peers: HashMap<uuid::Uuid, ManagedPeer> = HashMap::new();
        loop {
            tokio::select! {
                Some(augmented_info) = conn_info_rx.recv() => {
                    if socket.connection_info().public_key == augmented_info.connection_info.public_key {
                        // Don't try to connect to self.
                        continue;
                    }
                    let id = snow_public_keys_to_uuid(&socket.connection_info().public_key, &augmented_info.connection_info.public_key);
                    if let Some(managed_peer) = update_peer_info(
                        id, augmented_info,
                        socket.clone(),
                        app_event_tx.clone(),
                        &mut managed_peers,
                        sender_is_muted.clone()) {
                        log::debug!("Updated peer info for {id} to: {:?}", managed_peer.info());
                        log::debug!("(Re)Connecting to peer {id}.");
                        reconnect(managed_peer);
                    }
                },
                Some(user_action) = user_action_rx.recv() => {
                    if let Err(e) = handle_user_action(user_action, sender_is_muted.clone(), app_event_tx.clone(), &mut managed_peers) {
                        log::debug!("Failed to handle user action: {:?}", e);
                    }
                }
                _ = cancellation_token.cancelled() => {
                    log::debug!("Peer connector shutdown.");
                    break;
                }
            }
        }
    });
    conn_info_tx
}

fn reconnect(managed_peer: ManagedPeer) {
    match managed_peer.connection_status() {
        ConnectionStatus::Disabled => {
            managed_peer.enable();
        }
        ConnectionStatus::Connecting | ConnectionStatus::Connected => {
            match managed_peer.disable() {
                Ok(()) => {
                    managed_peer.enable();
                }
                Err(e) => {
                    log::debug!("Failed to disable peer, so couldn't re-enable: {:?}", e);
                }
            }
        }
    }
}

/// Returns Some containing the old peer and the updated peer, or None if no peer updated.
fn update_peer_info(
    id: uuid::Uuid,
    new_info: AugmentedInfo,
    socket: veq::veq::VeqSocket,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    managed_peers: &mut HashMap<uuid::Uuid, ManagedPeer>,
    sender_is_muted: Arc<AtomicBool>,
) -> Option<ManagedPeer> {
    match managed_peers.get_mut(&id) {
        Some(current_managed_peer) => {
            // If already have this peer, update the managed peer as necessary.
            if current_managed_peer.info() != new_info {
                current_managed_peer.set_info(new_info);
                Some(current_managed_peer.clone())
            } else {
                None
            }
        }
        None => {
            // If new peer, add to managed peers.
            let managed_peer = ManagedPeer::builder()
                .id(id)
                .connection_info(new_info.connection_info)
                .socket(socket)
                .maybe_app_event_tx(app_event_tx)
                .display_name(new_info.display_name)
                .denoise(true)
                .volume(100)
                .sender_is_muted(sender_is_muted)
                .build();
            managed_peers.insert(id, managed_peer.clone());
            Some(managed_peer)
        }
    }
}

fn handle_user_action(
    user_action: UserInputEvent,
    sender_is_muted: Arc<AtomicBool>,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    managed_peers: &mut HashMap<uuid::Uuid, ManagedPeer>,
) -> anyhow::Result<()> {
    match user_action {
        UserInputEvent::DisableDenoise(id) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.set_denoise(false)?;
            }
        }
        UserInputEvent::EnableDenoise(id) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.set_denoise(true)?;
            }
        }
        UserInputEvent::DisablePeer(id) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.disable()?;
            }
        }
        UserInputEvent::EnablePeer(id) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.enable();
            }
        }
        UserInputEvent::SetVolume(id, volume) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.set_volume(volume)?;
            }
        }
        UserInputEvent::SendMessage(message) => {
            for (_, peer) in managed_peers.iter() {
                if let Err(e) = peer.send_message(message.clone()) {
                    log::debug!("Failed to send message to a peer: {:?}", e);
                }
            }
        }
        UserInputEvent::SetMuteSelf(is_muted) => {
            sender_is_muted.store(is_muted, Ordering::Relaxed);
            if let Some(app_event_tx) = app_event_tx {
                if let Err(e) = app_event_tx.send(AppEvent::MuteSelf(is_muted)) {
                    log::debug!("Failed to send mute self event: {:?}", e);
                }
            }
        }
    }
    Ok(())
}

// This converts snow public keys to strings and then does what
// onion_addresses_to_uuid from the old code did.
// Absolutely no clue what this is for.
// TODO: why
fn snow_public_keys_to_uuid(key1: &SnowPublicKey, key2: &SnowPublicKey) -> uuid::Uuid {
    let key1_str = key1.clone().base64().to_string();
    let key2_str = key2.clone().base64().to_string();

    let lower_str = std::cmp::min(key1_str.clone(), key2_str.clone());
    let higher_str = std::cmp::max(key1_str, key2_str);

    let mut hasher = Sha256::new();
    hasher.update(lower_str.as_bytes());
    hasher.update(higher_str.as_bytes());
    let result = hasher.finalize();
    let mut dest = [0u8; 16];
    dest.clone_from_slice(&result[0..16]);
    uuid::Uuid::from_bytes(dest)
}

fn get_or_make_keypair(db: &sled::Db) -> anyhow::Result<SnowKeypair> {
    match db
        .get(DB_KEY_PRIVATE_KEY)?
        .and_then(|v| bincode::deserialize::<SnowKeypair>(&v).ok())
    {
        Some(keypair) => {
            log::info!(
                "Found keypair in database. Public Key: {:?}",
                keypair.public()
            );
            Ok(keypair)
        }
        None => {
            log::info!("No keypair found in db, generating one");
            let keypair = SnowKeypair::new().expect("Failed to generate keypair");
            db.insert(DB_KEY_PRIVATE_KEY, bincode::serialize(&keypair)?)?;
            Ok(keypair)
        }
    }
}
