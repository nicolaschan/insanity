use std::{sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
}};

use insanity_tui::{AppEvent, Peer, PeerState};
use itertools::Itertools;
use tokio::sync::{broadcast, mpsc};
use veq::veq::VeqSocket;

use crate::{
    clerver::start_clerver,
    protocol::{ConnectionManager, OnionAddress, ProtocolMessage},
};

pub struct ManagedPeer {
    address: OnionAddress,
    denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    socket: VeqSocket,
    conn_manager: Arc<ConnectionManager>,
    ui_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    peer_message_sender: broadcast::Sender<ProtocolMessage>,
    shutdown_tx: broadcast::Sender<()>,
    enabled: Arc<AtomicBool>,
}

impl ManagedPeer {
    pub async fn new(
        address: OnionAddress,
        denoise: bool,
        volume: usize,
        socket: VeqSocket,
        conn_manager: Arc<ConnectionManager>,
        ui_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    ) -> Self {
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(10);
        let (peer_message_sender, _) = broadcast::channel(10);
        Self {
            address,
            denoise: Arc::new(AtomicBool::new(denoise)),
            volume: Arc::new(Mutex::new(volume)),
            socket,
            conn_manager,
            ui_sender,
            peer_message_sender,
            shutdown_tx,
            enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn set_denoise(&self, denoise: bool) {
        self.denoise.store(denoise, Ordering::Relaxed);
        if let Some(sender) = &self.ui_sender {
            sender.send(AppEvent::SetPeerDenoise(self.address.to_string(), denoise)).unwrap();
        }
    }

    pub async fn set_volume(&self, volume: usize) {
        let ui_sender = self.ui_sender.clone();
        let address = self.address.to_string();
        let mut volume_guard = self.volume.lock().unwrap();
        *volume_guard = volume;
        if let Some(sender) = ui_sender {
            sender.send(AppEvent::SetPeerVolume(address, volume)).unwrap();
        }
    }

    pub fn send_message(&self, message: String) {
        let protocol_message = ProtocolMessage::ChatMessage(message);
        if self.peer_message_sender.receiver_count() > 0 {
            self.peer_message_sender.send(protocol_message).unwrap();
        }
    }

    pub async fn enable(&self) {
        let address = self.address.clone();
        let conn_manager = self.conn_manager.clone();
        let ui_sender = self.ui_sender.clone();
        let peer_message_sender = self.peer_message_sender.clone();
        let denoise = self.denoise.clone();
        let volume = self.volume.clone();
        let display_name = conn_manager.cached_display_name(&address);
        
        let peer = Peer::new(
            address.to_string(),
            display_name,
            PeerState::Disconnected,
            self.denoise.load(Ordering::Relaxed),
            *self.volume.lock().unwrap(),
        );

        if let Some(sender) = &self.ui_sender {
            sender.send(AppEvent::AddPeer(peer.clone())).unwrap();
        }
        
        let socket = self.socket.clone();

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let (inner_tx, _inner_rx) = broadcast::channel(10);
            loop {
                log::info!("Beginning connect loop to peer {}", address);
                tokio::select! {
                    _ = tokio::spawn(connect(address.clone(), conn_manager.clone(), ui_sender.clone(), peer_message_sender.subscribe(), denoise.clone(), volume.clone(), socket.clone(), inner_tx.subscribe())) => {},
                    _ = shutdown_rx.recv() => {
                        inner_tx.send(()).unwrap();
                        break;
                    }
                }
                log::info!("Lost connection to peer {}", address);
                if let Some(sender) = &ui_sender {
                    sender.send(AppEvent::AddPeer(peer.clone())).unwrap();
                }
            }
        });
    }

    pub async fn disable(&self) {
        log::info!("Disable peer {:?}", self.address);
        if self.shutdown_tx.send(()).is_ok() {
            log::info!("Disabled peer {:?}", self.address);
        } else {
            log::info!(
                "Failed to disable peer {:?}. Peer likely not running in the first place.",
                self.address
            );
        }
        self.enabled.store(false, Ordering::Relaxed);
        if let Some(sender) = &self.ui_sender {
            sender
                .send(AppEvent::AddPeer(Peer::new(
                    self.address.to_string(),
                    None,
                    PeerState::Disabled,
                    self.denoise.load(Ordering::Relaxed),
                    *self.volume.lock().unwrap(),
                )))
                .unwrap();
        }
    }
}

async fn connect(
    address: OnionAddress,
    conn_manager: Arc<ConnectionManager>,
    ui_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    peer_message_receiver: broadcast::Receiver<ProtocolMessage>,
    denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    mut socket: VeqSocket,
    mut shutdown_receiver: broadcast::Receiver<()>,
) {
    log::info!("Connecting to peer {:?}", address);
    if let Some((session, info)) = tokio::select! {
        res = conn_manager.session(&mut socket, &address) => res,
        _x = async {
            if let Some(ref sender) = ui_sender {
                let mut index = 0;
                loop {
                    if let Some(cached_peer_info) = conn_manager.cached_peer_info(&address) {
                        let ip_addresses_sorted = cached_peer_info.conn_info.addresses.iter().sorted().collect::<Vec<_>>();
                        let ip_address = ip_addresses_sorted.get(index).map(|x| x.to_string()).unwrap_or("".to_string());
                        sender.send(AppEvent::AddPeer(Peer::new(
                            address.clone().to_string(),
                            Some(cached_peer_info.display_name.clone()),
                            PeerState::Connecting(ip_address),
                            denoise.load(Ordering::Relaxed),
                            *volume.lock().unwrap(),
                        ))).unwrap();
                        index = (index + 1) % ip_addresses_sorted.len();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        } => { return; },
        _ = shutdown_receiver.recv() => { return; }
    } {
        log::info!("Connected to peer {:?}", address);
        if let Some(ref sender) = ui_sender {
            sender
                .send(AppEvent::AddPeer(Peer::new(
                    address.clone().to_string(),
                    Some(info.display_name.clone()),
                    PeerState::Connected(session.remote_addr().await.to_string()),
                    denoise.load(Ordering::Relaxed),
                    *volume.lock().unwrap(),
                )))
                .unwrap();
        }
        start_clerver(
            session,
            ui_sender,
            peer_message_receiver,
            denoise.clone(),
            volume,
            address.clone(),
            shutdown_receiver,
        )
        .await;
        log::info!("Connection closed with {}", address);
    }
}
