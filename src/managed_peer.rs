use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use desync::Desync;
use insanity_tui::{AppEvent, Peer, PeerState};
use tokio::{
    sync::{
        broadcast::{self, Receiver},
        mpsc::{UnboundedSender},
    },
};
use veq::veq::{VeqSocket};

use crate::{
    clerver::start_clerver,
    protocol::{ConnectionManager, OnionAddress},
};

pub struct ManagedPeer {
    address: OnionAddress,
    denoise: Arc<AtomicBool>,
    volume: Arc<Desync<usize>>,
    socket: VeqSocket,
    conn_manager: Arc<ConnectionManager>,
    ui_sender: Option<UnboundedSender<AppEvent>>,
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
        ui_sender: Option<UnboundedSender<AppEvent>>,
    ) -> Self {
        let (tx, _rx) = broadcast::channel(10);
        Self {
            address,
            denoise: Arc::new(AtomicBool::new(denoise)),
            volume: Arc::new(Desync::new(volume)),
            socket,
            conn_manager,
            ui_sender,
            shutdown_tx: tx,
            enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn set_denoise(&self, denoise: bool) {
        self.denoise.store(denoise, Ordering::Relaxed);
        if let Some(sender) = &self.ui_sender {
            sender
                .send(AppEvent::SetPeerDenoise(self.address.to_string(), denoise))
                .unwrap();
        }
    }

    pub async fn set_volume(&self, volume: usize) {
        let ui_sender = self.ui_sender.clone();
        let address = self.address.to_string();
        self.volume.desync(move |v| {
            *v = volume;
            if let Some(sender) = ui_sender {
                sender
                    .send(AppEvent::SetPeerVolume(address, volume))
                    .unwrap();
            }
        });
    }

    pub async fn enable(&self) {
        if let Some(sender) = &self.ui_sender {
            sender
                .send(AppEvent::AddPeer(Peer::new(
                    self.address.to_string(),
                    None,
                    PeerState::Disconnected,
                    self.denoise.load(Ordering::Relaxed),
                    self.volume.sync(|v| *v),
                )))
                .unwrap();
        }

        let address = self.address.clone();
        let conn_manager = self.conn_manager.clone();
        let ui_sender = self.ui_sender.clone();
        let denoise = self.denoise.clone();
        let volume = self.volume.clone();
        let socket = self.socket.clone();

        let mut rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let (inner_tx, _inner_rx) = broadcast::channel(10);
            loop {
                log::info!("Beginning connect loop to peer {}", address);
                tokio::select! {
                    _ = tokio::spawn(connect(address.clone(), conn_manager.clone(), ui_sender.clone(), denoise.clone(), volume.clone(), socket.clone(), inner_tx.subscribe())) => {},
                    _ = rx.recv() => {
                        inner_tx.send(()).unwrap();
                        break;
                    }
                }
                log::info!("Lost connection to peer {}", address);
                if let Some(sender) = &ui_sender {
                    sender
                        .send(AppEvent::AddPeer(Peer::new(
                            address.to_string(),
                            None,
                            PeerState::Disconnected,
                            denoise.load(Ordering::Relaxed),
                            volume.sync(|v| *v),
                        )))
                        .unwrap();
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
                    self.volume.sync(|v| *v),
                )))
                .unwrap();
        }
    }
}

async fn connect(
    address: OnionAddress,
    conn_manager: Arc<ConnectionManager>,
    ui_sender: Option<UnboundedSender<AppEvent>>,
    denoise: Arc<AtomicBool>,
    volume: Arc<Desync<usize>>,
    mut socket: VeqSocket,
    mut rx: Receiver<()>,
) {
    log::info!("Connecting to peer {:?}", address);
    if let Some((session, info)) = tokio::select! {
        res = conn_manager.session(&mut socket, &address) => res,
        _ = rx.recv() => { return; }
    } {
        log::info!("Connected to peer {:?}", address);
        if let Some(sender) = &ui_sender {
            sender
                .send(AppEvent::AddPeer(Peer::new(
                    address.to_string(),
                    Some(info.display_name),
                    PeerState::Connected(session.remote_addr().await.to_string()),
                    denoise.load(Ordering::Relaxed),
                    volume.sync(|v| *v),
                )))
                .unwrap();
        }
        start_clerver(session, denoise.clone(), volume, rx).await;
        log::info!("Connection closed with {}", address);
    }
}
