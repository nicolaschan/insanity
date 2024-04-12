use std::{collections::HashMap, path::PathBuf, sync::atomic::Ordering};

use insanity_tui::{AppEvent, Peer, PeerState, UserAction};
use iroh::{
    client::{Doc, Iroh},
    node::Node,
    sync::{
        store::{DownloadPolicy, FilterKind, Query},
        AuthorId,
    },
    ticket::DocTicket,
};
use sha2::{Digest, Sha256};
use veq::{snow_types::SnowKeypair, veq::VeqSocket};

use iroh::rpc_protocol::ProviderService;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::{clerver::start_clerver, managed_peer::ManagedPeer, session::UpdatablePendingSession};
use veq::snow_types::SnowPublicKey;

const IROH_KEY_INFO: &'static str = "info";
const IROH_KEY_HEARTBEAT: &'static str = "heartbeat";
const IROH_KEY_LIST: [&'static str; 2] = [IROH_KEY_INFO, IROH_KEY_HEARTBEAT];

const IROH_VALUE_HEARTBEAT: &'static str = "alive";

const DB_KEY_PRIVATE_KEY: &'static str = "private_key";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AugmentedInfo {
    pub connection_info: veq::veq::ConnectionInfo,
    pub display_name: String,
}

pub struct ConnectionManager {
    socket: VeqSocket,
    db: sled::Db,
    cancellation_token: CancellationToken,
    // TODO: refactor so user_action_tx is not optional.
    user_action_tx: Option<mpsc::UnboundedSender<UserAction>>,
}

impl ConnectionManager {
    pub fn builder(base_dir: PathBuf, listen_port: u16) -> ConnectionManagerBuilder {
        return ConnectionManagerBuilder::new(base_dir, listen_port);
    }

    pub fn shutdown(&self) {
        self.cancellation_token.cancel();
    }

    pub fn send_user_action(&self, action: UserAction) -> anyhow::Result<()> {
        if let &Some(ref user_action_tx) = &self.user_action_tx {
            Ok(user_action_tx.send(action)?)
        } else {
            Err(anyhow::anyhow!("No user_action_tx"))
        }
    }

    async fn start(
        &mut self,
        room_ticket: Option<String>,
        base_dir: PathBuf,
        display_name: Option<String>,
        app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    ) -> anyhow::Result<()> {
        let connection_info = self.socket.connection_info();
        log::debug!("Connection info: {:?}", connection_info);

        let (conn_info_tx, user_action_tx) = manage_peers(
            self.socket.clone(),
            app_event_sender,
            self.cancellation_token.clone(),
        );

        self.user_action_tx = Some(user_action_tx);

        if let &Some(ref room_ticket) = &room_ticket {
            log::debug!("Attempting to join room {room_ticket}.");
            let iroh_path = base_dir.join("iroh");
            start_room_connection(
                room_ticket,
                connection_info,
                display_name,
                &iroh_path,
                conn_info_tx,
                self.cancellation_token.clone(),
            )
            .await?;
        } else {
            log::debug!("Not joining any room.");
        }

        Ok(())
    }
}

pub struct ConnectionManagerBuilder {
    base_dir: PathBuf,
    listen_port: u16,
    room_ticket: Option<String>,
    display_name: Option<String>,
    cancellation_token: Option<CancellationToken>,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
}

impl ConnectionManagerBuilder {
    pub fn new(base_dir: PathBuf, listen_port: u16) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            base_dir,
            listen_port,
            room_ticket: None,
            display_name: None,
            cancellation_token: None,
            app_event_sender: None,
        }
    }

    pub fn room(self, room_ticket: Option<String>) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            room_ticket,
            ..self
        }
    }

    pub fn display_name(self, display_name: Option<String>) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            display_name,
            ..self
        }
    }

    pub fn cancellation_token(
        self,
        cancellation_token: Option<CancellationToken>,
    ) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            cancellation_token,
            ..self
        }
    }

    pub fn app_event_sender(
        self,
        app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    ) -> ConnectionManagerBuilder {
        ConnectionManagerBuilder {
            app_event_sender,
            ..self
        }
    }

    /// Creates the local socket, uploads connection info, and begins searching for connections.
    pub async fn start(self) -> anyhow::Result<ConnectionManager> {
        let cancellation_token = self.cancellation_token.unwrap_or(CancellationToken::new());

        // Create or open connection manager database.
        let sled_path = self.base_dir.join("connection_manager_data.sled");
        let db = sled::open(sled_path)?;

        // Create local socket.
        let keypair: SnowKeypair = get_or_make_keypair(&db)?;
        let socket = VeqSocket::bind_with_keypair(
            (std::net::Ipv6Addr::LOCALHOST, self.listen_port),
            keypair,
        )
        .await?;

        let mut connection_manager = ConnectionManager {
            socket,
            db,
            cancellation_token,
            user_action_tx: None,
        };
        connection_manager
            .start(
                self.room_ticket,
                self.base_dir,
                self.display_name,
                self.app_event_sender,
            )
            .await?;
        Ok(connection_manager)
    }
}

/// Receive peer augmented info over channel and connect to peer.
fn manage_peers(
    socket: veq::veq::VeqSocket,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    cancellation_token: CancellationToken,
) -> (
    mpsc::UnboundedSender<AugmentedInfo>,
    mpsc::UnboundedSender<UserAction>,
) {
    // Channel for the manage_peers task to receive updated peers info.
    let (conn_info_tx, mut conn_info_rx) = mpsc::unbounded_channel::<AugmentedInfo>();
    let (user_action_tx, mut user_action_rx) = mpsc::unbounded_channel::<UserAction>();
    tokio::spawn(async move {
        let mut managed_peers: HashMap<uuid::Uuid, ManagedPeer> = HashMap::new();
        loop {
            tokio::select! {
                Some(augmented_info) = conn_info_rx.recv() => {
                    let id = snow_public_keys_to_uuid(&socket.connection_info().public_key, &augmented_info.connection_info.public_key);
                    if let Some(managed_peer) = update_peer_info(id, augmented_info, &mut managed_peers) {
                        if let &Some(ref sender ) = &app_event_sender {
                            let peer = Peer::new(
                                id.to_string(),
                                Some(managed_peer.display_name.clone()),
                                PeerState::Disconnected,
                                managed_peer.denoise.load(Ordering::Relaxed),
                                *managed_peer.volume.lock().unwrap(),
                            );

                            log::debug!("Sending AppEvent AddPeer: {:?}", peer.clone());
                            // TODO: ensure that TUI correctly handles repeated AddPeer for same ID.
                            sender.send(AppEvent::AddPeer(peer)).unwrap();
                        }

                        start_connection(socket.clone(), managed_peer, app_event_sender.clone(), cancellation_token.clone());
                    }
                },
                Some(user_action) = user_action_rx.recv() => {
                    if let Err(e) = handle_user_action(user_action, &mut managed_peers, &app_event_sender) {
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
    (conn_info_tx, user_action_tx)
}

fn handle_user_action(
    user_action: UserAction,
    managed_peers: &mut HashMap<uuid::Uuid, ManagedPeer>,
    app_event_sender: &Option<mpsc::UnboundedSender<AppEvent>>,
) -> anyhow::Result<()> {
    match user_action {
        UserAction::DisableDenoise(id) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.set_denoise(false);
                if let &Some(ref app_event_tx) = app_event_sender {
                    app_event_tx.send(AppEvent::SetPeerDenoise(id.to_string(), false))?;
                }
            }
        }
        UserAction::EnableDenoise(id) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.set_denoise(true);
                if let &Some(ref app_event_tx) = app_event_sender {
                    app_event_tx.send(AppEvent::SetPeerDenoise(id.to_string(), true))?;
                }
            }
        }
        UserAction::DisablePeer(id) => {
            // TODO: need to actually implement this
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                if let Err(e) = peer.disable() {
                    log::debug!("Failed to disable peer {id}: {:?}", e);
                } else {
                    if let &Some(ref app_event_tx) = app_event_sender {
                        app_event_tx.send(AppEvent::AddPeer(Peer::new(
                            id.to_string(),
                            Some(peer.display_name.clone()),
                            PeerState::Disabled,
                            peer.denoise.load(Ordering::Relaxed),
                            *peer.volume.lock().unwrap(),
                        )))?;
                    }
                }
            }
        }
        UserAction::EnablePeer(id) => {
            // TODO: need to actually implement this.
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.enable();
            }
        }
        UserAction::SetVolume(id, volume) => {
            let id = uuid::Uuid::from_str(&id)?;
            if let Some(peer) = managed_peers.get(&id) {
                peer.set_volume(volume);
                if let &Some(ref app_event_tx) = app_event_sender {
                    app_event_tx.send(AppEvent::SetPeerVolume(id.to_string(), volume))?;
                }
            }
        }
        UserAction::SendMessage(message) => {
            for (_, peer) in managed_peers.iter() {
                if let Err(e) = peer.send_message(message.clone()) {
                    log::debug!("Failed to send message to a peer: {:?}", e);
                }
            }
        }
    }
    Ok(())
}

/// Returns Some containing an updated peer, or None if no peer updated.
fn update_peer_info(
    id: uuid::Uuid,
    info: AugmentedInfo,
    managed_peers: &mut HashMap<uuid::Uuid, ManagedPeer>,
) -> Option<ManagedPeer> {
    let AugmentedInfo {
        connection_info,
        display_name,
    } = info;
    if managed_peers.contains_key(&id) {
        // If already have this peer, update the managed peer.
        // If any changes occur, restart connection to peer.
        // TODO: handle this case
        None
    } else {
        // If new peer, add to managed peers and start connection
        // TODO: use commandline argument for denoise default.
        let managed_peer = ManagedPeer::new(true, 100, connection_info, display_name);
        managed_peers.insert(id, managed_peer.clone());
        Some(managed_peer)
    }
}

fn start_connection(
    socket: veq::veq::VeqSocket,
    peer: ManagedPeer,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    cancellation_token: CancellationToken,
) {
    tokio::spawn(async move {
        let id = snow_public_keys_to_uuid(
            &socket.connection_info().public_key,
            &peer.connection_info.public_key,
        );
        loop {
            log::info!("Beginning connect loop to peer {id}");
            tokio::select! {
                res = async {
                    let pending_session = UpdatablePendingSession::new(socket.clone());
                    let info = peer.info();
                    pending_session.update(id, info).await;
                    let (session, _info) = pending_session.session().await;
                    Some(session)
                } => {
                    if let Some(session) = res {
                        // Start and block on clerver.
                        log::info!("Starting clerver for {id}.");
                        start_clerver(
                            session,
                            app_event_sender.clone(),
                            peer.peer_message_sender.subscribe(),
                            peer.denoise.clone(),
                            peer.volume.clone(),
                            id,
                            peer.shutdown_tx.subscribe(),
                        )
                        .await;
                    }
                    // Otherwise, restart connection loop.
                },
                _ = cancellation_token.cancelled() => {
                    log::debug!("Stopping connecting loop to {id}.");
                    break;
                }
            }
        }
    });
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

/// Find peer connection info on the Iroh document room_ticket
/// and send it over the conn_info_tx channel.
async fn start_room_connection(
    room_ticket: &str,
    connection_info: veq::veq::ConnectionInfo,
    display_name: Option<String>,
    iroh_path: &PathBuf,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
    cancellation_token: CancellationToken,
) -> anyhow::Result<()> {
    let iroh_node = Node::persistent(iroh_path).await?.spawn().await?;
    let iroh_client = iroh_node.client().clone();
    let author_id = if let Ok(Some(author_id)) = iroh_client.authors.list().await?.try_next().await
    {
        // Reuse existing author ID.
        author_id
    } else {
        // Create new author ID if no existing one available.
        iroh_client.authors.create().await?
    };
    log::debug!("Author ID: {author_id}");
    let doc_ticket = DocTicket::from_str(room_ticket)?;
    log::debug!("Room ticket decoded: {:?}", doc_ticket);

    let doc = iroh_client.docs.import(doc_ticket.clone()).await?;
    // Download values for only those keys which are needed.
    doc.set_download_policy(DownloadPolicy::NothingExcept(
        IROH_KEY_LIST
            .iter()
            .map(|key| FilterKind::Exact((*key).into()))
            .collect(),
    ))
    .await?;

    // Write own info to document.
    let info = AugmentedInfo {
        connection_info,
        display_name: display_name.clone().unwrap_or(author_id.to_string()),
    };
    let json = serde_json::to_string(&info)?;
    doc.set_bytes(author_id, IROH_KEY_INFO, json).await?;

    // Start background tasks which should not close until Insanity does.
    tokio::spawn(async move {
        tokio::select! {
            res = handle_iroh_events(iroh_client, &doc, conn_info_tx) => {
                log::error!("Iroh event handler shutdown unexpectedly: {:?}.", res);
            },
            res = send_iroh_heartbeat(author_id, &doc) => {
                log::error!("Iroh heartbeat sender shutdown unexpectedly: {:?}.", res);
            },
            res = iroh_node => {
                log::error!("Iroh node shutdown unexpectedly: {:?}.", res);
            },
            _ = cancellation_token.cancelled() => {
                log::debug!("Iroh-related tasks shutdown.");
            }
        }
    });
    Ok(())
}

async fn handle_iroh_events<C: quic_rpc::ServiceConnection<ProviderService>>(
    client: Iroh<C>,
    doc: &Doc<C>,
    conn_info_tx: mpsc::UnboundedSender<AugmentedInfo>,
) {
    loop {
        log::debug!("starting loop of handle Iroh events.");
        let Ok(mut event_stream) = doc.subscribe().await else {
            log::debug!("Failed to subscribe to Iroh document event stream.");
            continue;
        };
        let query = Query::key_exact(IROH_KEY_INFO).build();
        while let Ok(_event) = event_stream.try_next().await {
            log::debug!("Reading Iroh event.");
            // let Some(event) = event else {
            //     continue;
            // };
            // match event {
            //     LiveEvent::InsertLocal { .. }
            //     | LiveEvent::ContentReady { .. }
            //     | LiveEvent::InsertRemote {
            //         content_status: ContentStatus::Complete,
            //         ..
            //     } => {

            let Ok(mut entry_stream) = doc.get_many(query.clone()).await else {
                log::debug!("Failed to get Iroh entry stream.");
                continue;
            };
            while let Ok(Some(entry)) = entry_stream.try_next().await {
                let Ok(content) = entry.content_bytes(&client).await else {
                    log::debug!("Could not read contents of Iroh entry.");
                    continue;
                };
                let Ok(info) = serde_json::from_slice::<AugmentedInfo>(&content) else {
                    log::debug!("Failed to parse contents of Iroh entry into AugmentedInfo.");
                    continue;
                };
                log::debug!("Got info: {:?}", info);
                if let Err(e) = conn_info_tx.send(info) {
                    log::debug!(
                        "Failed to send received augmented connection info over channel: {:?}",
                        e
                    );
                }
            }
            // }
            // _ => {}
            // }
        }
    }
}

async fn send_iroh_heartbeat<C: quic_rpc::ServiceConnection<ProviderService>>(
    author_id: AuthorId,
    doc: &Doc<C>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        log::debug!("Writing heartbeat.");
        if let Err(e) = doc
            .set_bytes(author_id, IROH_KEY_HEARTBEAT, IROH_VALUE_HEARTBEAT)
            .await
        {
            log::debug!("Sending Iroh heartbeat failed: {:?}", e);
        }
    }
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
