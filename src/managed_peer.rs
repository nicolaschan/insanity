use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use insanity_tui::{AppEvent, Peer, PeerState};
use tokio::sync::{broadcast, mpsc};
use veq::veq::{VeqSessionAlias, VeqSocket};

use crate::{
    clerver::run_clerver, connection_manager::AugmentedInfo, protocol::ProtocolMessage,
    session::UpdatablePendingSession,
};

// TODO: switch from the shutdown broadcaster to a
// child of the connection manager cancellation token
#[derive(Clone)]
pub struct ManagedPeer {
    id: uuid::Uuid,
    pub connection_info: veq::veq::ConnectionInfo,
    socket: VeqSocket,
    pub shutdown_tx: broadcast::Sender<()>,
    pub peer_message_tx: broadcast::Sender<ProtocolMessage>,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    pub display_name: String,
    pub denoise: Arc<AtomicBool>,
    pub volume: Arc<Mutex<usize>>,
}

impl ManagedPeer {
    pub fn new(
        id: uuid::Uuid,
        connection_info: veq::veq::ConnectionInfo,
        socket: VeqSocket,
        app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
        display_name: String,
        denoise: bool,
        volume: usize,
    ) -> ManagedPeer {
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(10);
        let (peer_message_tx, _) = broadcast::channel(10);
        ManagedPeer {
            denoise: Arc::new(AtomicBool::new(denoise)),
            volume: Arc::new(Mutex::new(volume)),
            connection_info,
            display_name,
            shutdown_tx,
            peer_message_tx,
            socket,
            app_event_tx,
            id,
        }
    }

    pub fn info(&self) -> AugmentedInfo {
        AugmentedInfo {
            connection_info: self.connection_info.clone(),
            display_name: self.display_name.clone(),
        }
    }

    pub fn set_denoise(&self, denoise: bool) -> anyhow::Result<()> {
        self.denoise.store(denoise, Ordering::Relaxed);
        if let &Some(ref app_event_tx) = &self.app_event_tx {
            app_event_tx.send(AppEvent::SetPeerDenoise(self.id.to_string(), denoise))?;
        }
        Ok(())
    }

    pub fn set_volume(&self, volume: usize) -> anyhow::Result<()> {
        let mut volume_guard = self.volume.lock().unwrap();
        *volume_guard = volume;
        if let &Some(ref app_event_tx) = &self.app_event_tx {
            app_event_tx.send(AppEvent::SetPeerVolume(self.id.to_string(), volume))?;
        }
        Ok(())
    }

    pub fn send_message(&self, message: String) -> anyhow::Result<()> {
        let protocol_message = ProtocolMessage::ChatMessage(message);
        if self.peer_message_tx.receiver_count() > 0 {
            self.peer_message_tx.send(protocol_message)?;
        }
        Ok(())
    }

    pub fn enable(&self) {
        if let &Some(ref app_event_tx) = &self.app_event_tx {
            if let Err(e) = app_event_tx.send(AppEvent::AddPeer(Peer::new(
                self.id.to_string(),
                Some(self.display_name.clone()),
                PeerState::Disabled,
                self.denoise.load(Ordering::Relaxed),
                *self.volume.lock().unwrap(),
            ))) {
                log::debug!("Failed to send app event: {:?}", e);
            }
        }

        let id = self.id.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let peer = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = run_connection_loop(peer) => {
                    log::debug!("Connection loop to {id} ended early.");
                },
                _ = shutdown_rx.recv() => {
                    log::debug!("Stopping connection loop to {id}.");
                }
            }
        });
    }

    pub fn disable(&self) -> anyhow::Result<()> {
        if self.shutdown_tx.send(()).is_ok() {
            log::info!("Disabled peer: {}", self.id);

            if let &Some(ref app_event_tx) = &self.app_event_tx {
                if let Err(e) = app_event_tx.send(AppEvent::AddPeer(Peer::new(
                    self.id.to_string(),
                    Some(self.display_name.clone()),
                    PeerState::Disabled,
                    self.denoise.load(Ordering::Relaxed),
                    *self.volume.lock().unwrap(),
                ))) {
                    log::debug!("Failed to send app event: {:?}", e);
                }
            }

            Ok(())
        } else {
            Err(anyhow::anyhow!("Failed to disable peer {}", self.id))
        }
    }
}

/// Should never terminate.
async fn run_connection_loop(peer: ManagedPeer) {
    let ip_addresses: Vec<String> = peer
        .connection_info
        .addresses
        .iter()
        .map(|ip_addr| ip_addr.to_string())
        .collect();

    loop {
        log::info!("Beginning connect loop to peer {}", peer.id);
        let mut socket = peer.socket.clone();
        tokio::select! {
            session = socket.connect(peer.id, peer.info().connection_info.clone()) => {
            // session = connect(peer.id, peer.info(), peer.socket.clone()) => {
                // Start and block on clerver.
                if let Ok(session) = session {
                    log::debug!("Connected to {}", peer.id);
                    if let &Some(ref app_event_tx) = &peer.app_event_tx {
                        if let Err(e) = app_event_tx.send(AppEvent::AddPeer(Peer::new(
                            peer.id.to_string(),
                            Some(peer.display_name.clone()),
                            PeerState::Connected(session.remote_addr().await.to_string()),
                            peer.denoise.load(Ordering::Relaxed),
                            *peer.volume.lock().unwrap(),
                        ))) {
                            log::debug!("Failed to send app event: {:?}", e);
                        }
                    }

                    log::info!("Starting clerver for connection with {}.", peer.id);
                    run_clerver(
                        session,
                        peer.app_event_tx.clone(),
                        peer.peer_message_tx.subscribe(),
                        peer.denoise.clone(),
                        peer.volume.clone(),
                        peer.id,
                    )
                    .await;
                }
            },
            _ = update_app_connecting_status(
                peer.id,
                peer.display_name.clone(),
                peer.denoise.clone(),
                peer.volume.clone(),
                ip_addresses.clone(),
                peer.app_event_tx.clone()
            ) => {
                log::debug!("Connecting status updater ended early.");
             },
        }
    }
}

/// Connects the local socket to peer identified by id and info.
async fn connect(
    id: uuid::Uuid,
    info: AugmentedInfo,
    socket: veq::veq::VeqSocket,
) -> VeqSessionAlias {
    let pending_session = UpdatablePendingSession::new(socket);
    pending_session.update(id, info).await;
    log::debug!("Updated pending session of {id}.");
    let (session, _info) = pending_session.session().await;
    session
}

/// Cycles through connection info and sends to app. Should never terminate.
async fn update_app_connecting_status(
    id: uuid::Uuid,
    display_name: String,
    denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    ip_addresses: Vec<String>,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
) {
    if let Some(app_event_tx) = app_event_tx {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(1000));
        loop {
            for ip_address in ip_addresses.iter() {
                interval.tick().await;
                if let Err(e) = app_event_tx.send(AppEvent::AddPeer(Peer::new(
                    id.to_string(),
                    Some(display_name.clone()),
                    PeerState::Connecting(ip_address.clone()),
                    denoise.load(Ordering::Relaxed),
                    *volume.lock().unwrap(),
                ))) {
                    log::debug!("Failed to send app event: {:?}", e);
                }
            }
        }
    } else {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10000)).await;
        }
    }
}
