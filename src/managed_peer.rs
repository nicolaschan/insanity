use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use insanity_tui::{AppEvent, Peer, PeerState};
use tokio::sync::{broadcast, mpsc};
use veq::veq::VeqSocket;

use crate::{clerver::run_clerver, connection_manager::AugmentedInfo, protocol::ProtocolMessage};

#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    Disabled,
    Connecting,
    Connected,
}

#[derive(Clone)]
pub struct ManagedPeer {
    id: uuid::Uuid,
    connection_info: veq::veq::ConnectionInfo,
    socket: VeqSocket,
    shutdown_tx: broadcast::Sender<()>,
    peer_message_tx: broadcast::Sender<ProtocolMessage>,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    connection_status: Arc<Mutex<ConnectionStatus>>,
    display_name: String,
    denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
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
            connection_status: Arc::new(Mutex::new(ConnectionStatus::Disabled)),
        }
    }

    pub fn set_info(&mut self, info: AugmentedInfo) {
        self.connection_info = info.connection_info;
        self.display_name = info.display_name;
    }

    pub fn info(&self) -> AugmentedInfo {
        AugmentedInfo {
            connection_info: self.connection_info.clone(),
            display_name: self.display_name.clone(),
        }
    }

    pub fn connection_status(&self) -> ConnectionStatus {
        let connection_status = self.connection_status.lock().unwrap();
        connection_status.clone()
    }

    pub fn set_denoise(&self, denoise: bool) -> anyhow::Result<()> {
        self.denoise.store(denoise, Ordering::Relaxed);
        if let Some(ref app_event_tx) = &self.app_event_tx {
            app_event_tx.send(AppEvent::SetPeerDenoise(self.id.to_string(), denoise))?;
        }
        Ok(())
    }

    pub fn set_volume(&self, volume: usize) -> anyhow::Result<()> {
        let mut volume_guard = self.volume.lock().unwrap();
        *volume_guard = volume;
        if let Some(ref app_event_tx) = &self.app_event_tx {
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
        let id = self.id;
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
        self.shutdown_tx.send(())?;
        log::info!("Disabled peer: {}", self.id);

        if let Ok(mut connection_status) = self.connection_status.lock() {
            *connection_status = ConnectionStatus::Disabled;
        }

        if let Some(ref app_event_tx) = &self.app_event_tx {
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

        if let Ok(mut connection_status) = peer.connection_status.lock() {
            *connection_status = ConnectionStatus::Connecting;
        }

        let mut socket = peer.socket.clone();
        tokio::select! {
            session = socket.connect(peer.id, peer.info().connection_info.clone()) => {
                // Start and block on clerver.
                if let Ok(session) = session {
                    log::debug!("Connected to {}", peer.id);

                    if let Ok(mut connection_status) = peer.connection_status.lock() {
                        *connection_status = ConnectionStatus::Connected;
                    }

                    if let Some(ref app_event_tx) = &peer.app_event_tx {
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

/// Cycles through connection info and sends to app. Should never terminate.
async fn update_app_connecting_status(
    id: uuid::Uuid,
    display_name: String,
    denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    ip_addresses: Vec<String>,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
) {
    if ip_addresses.is_empty() {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10000)).await;
        }
    } else if let Some(app_event_tx) = app_event_tx {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
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
