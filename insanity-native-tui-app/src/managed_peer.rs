use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU32, Ordering},
};

use bon::bon;
use insanity_core::audio::AudioFormat;
use insanity_core::audio::mixer::DEFAULT_OUT_FRAMES;
use insanity_core::user_input_event::DenoiseSelection;
use insanity_tui_adapter::{AppEvent, Peer, PeerState};
use tokio::sync::{broadcast, mpsc};
use veq::veq::VeqSocket;

use crate::{
    audio::{
        AudioInputHub, MixerClient, NO_SLOT, PeerControls, output_resampler, rebuild_opus_decoder,
    },
    clerver::run_clerver,
    connection_manager::AugmentedInfo,
    protocol::ProtocolMessage,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnectionStatus {
    Disabled = 0,
    Connecting = 1,
    Connected = 2,
}

impl ConnectionStatus {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => ConnectionStatus::Connecting,
            2 => ConnectionStatus::Connected,
            _ => ConnectionStatus::Disabled,
        }
    }
}

#[derive(Clone)]
pub struct ManagedPeer {
    id: uuid::Uuid,
    connection_info: veq::veq::ConnectionInfo,
    socket: VeqSocket,
    shutdown_tx: broadcast::Sender<()>,
    peer_message_tx: broadcast::Sender<ProtocolMessage>,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    connection_status: Arc<AtomicU8>,
    display_name: String,
    controls: PeerControls,
    slot: Arc<AtomicU32>,
    out_format: AudioFormat,
    hub: Arc<AudioInputHub>,
    client: MixerClient,
}

#[bon]
impl ManagedPeer {
    #[builder]
    pub fn new(
        id: uuid::Uuid,
        connection_info: veq::veq::ConnectionInfo,
        socket: VeqSocket,
        app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
        display_name: String,
        denoise: DenoiseSelection,
        volume: usize,
        out_format: AudioFormat,
        hub: Arc<AudioInputHub>,
        client: MixerClient,
    ) -> ManagedPeer {
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(10);
        let (peer_message_tx, _) = broadcast::channel(10);
        ManagedPeer {
            connection_info,
            display_name,
            shutdown_tx,
            peer_message_tx,
            socket,
            app_event_tx,
            id,
            controls: PeerControls::new(volume, denoise),
            slot: Arc::new(AtomicU32::new(NO_SLOT)),
            out_format,
            hub,
            client,
            connection_status: Arc::new(AtomicU8::new(ConnectionStatus::Disabled as u8)),
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
        ConnectionStatus::from_u8(self.connection_status.load(Ordering::Acquire))
    }

    pub fn set_denoise(&self, denoise: DenoiseSelection) -> anyhow::Result<()> {
        self.controls.denoise.set(denoise);
        if let Some(app_event_tx) = &self.app_event_tx {
            app_event_tx.send(AppEvent::SetPeerDenoise(self.id.to_string(), denoise))?;
        }
        Ok(())
    }

    pub fn set_volume(&self, volume: usize) -> anyhow::Result<()> {
        self.controls.gain.set(volume);
        let clamped = self.controls.gain.get();
        if let Some(app_event_tx) = &self.app_event_tx {
            app_event_tx.send(AppEvent::SetPeerVolume(self.id.to_string(), clamped))?;
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

    pub async fn disable(&self) -> anyhow::Result<()> {
        self.unsubscribe_mixer().await;
        self.shutdown_tx.send(())?;
        log::info!("Disabled peer: {}", self.id);

        self.connection_status
            .store(ConnectionStatus::Disabled as u8, Ordering::Release);

        if let Some(app_event_tx) = &self.app_event_tx
            && let Err(e) = app_event_tx.send(AppEvent::AddPeer(Peer::new(
                self.id.to_string(),
                Some(self.display_name.clone()),
                PeerState::Disabled,
                self.controls.denoise.get(),
                self.controls.gain.get(),
            )))
        {
            log::debug!("Failed to send app event: {:?}", e);
        }

        Ok(())
    }

    async fn subscribe_mixer(&self) {
        self.unsubscribe_mixer().await;
        let resampler = output_resampler(self.out_format.clone(), DEFAULT_OUT_FRAMES);
        let slot = self
            .client
            .subscribe(self.controls.clone(), rebuild_opus_decoder, resampler)
            .await
            .unwrap_or(NO_SLOT);
        self.slot.store(slot, Ordering::Release);
    }

    async fn unsubscribe_mixer(&self) {
        let slot = self.slot.swap(NO_SLOT, Ordering::AcqRel);
        if slot != NO_SLOT {
            self.client.unsubscribe(slot).await;
        }
    }

    fn audio_endpoint(&self) -> Option<(MixerClient, u32)> {
        let slot = self.slot.load(Ordering::Acquire);
        if slot == NO_SLOT {
            return None;
        }
        Some((self.client.clone(), slot))
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

        peer.connection_status
            .store(ConnectionStatus::Connecting as u8, Ordering::Release);

        let mut socket = peer.socket.clone();
        tokio::select! {
            session = socket.connect(peer.id, peer.info().connection_info.clone()) => {
                // Start and block on clerver.
                if let Ok(session) = session {
                    log::debug!("Connected to {}", peer.id);

                    peer.connection_status
                        .store(ConnectionStatus::Connected as u8, Ordering::Release);

                    if let Some(app_event_tx) = &peer.app_event_tx
                        && let Err(e) = app_event_tx.send(AppEvent::AddPeer(Peer::new(
                            peer.id.to_string(),
                            Some(peer.display_name.clone()),
                            PeerState::Connected(session.remote_addr().await.to_string()),
                            peer.controls.denoise.get(),
                            peer.controls.gain.get(),
                        ))) {
                            log::debug!("Failed to send app event: {:?}", e);
                        }

                    log::info!("Starting clerver for connection with {}.", peer.id);
                    // register peer with single output mixer before streaming
                    peer.subscribe_mixer().await;
                    let Some((client, slot)) = peer.audio_endpoint() else {
                        continue;
                    };
                    let loudness = peer.controls.loudness.clone();
                    run_clerver(
                        session,
                        peer.app_event_tx.clone(),
                        peer.hub.clone(),
                        |frame| client.push_frame(slot, frame),
                        loudness,
                        peer.id.to_string(),
                        peer.peer_message_tx.subscribe(),
                    )
                    .await;
                    peer.unsubscribe_mixer().await;
                }
            },
            _ = update_app_connecting_status(
                peer.id,
                peer.display_name.clone(),
                peer.controls.denoise.get(),
                peer.controls.gain.get(),
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
    denoise: DenoiseSelection,
    volume: usize,
    ip_addresses: Vec<String>,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
) {
    if ip_addresses.is_empty() {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10000)).await;
        }
    } else {
        match app_event_tx {
            Some(app_event_tx) => {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
                loop {
                    for ip_address in ip_addresses.iter() {
                        interval.tick().await;
                        if let Err(e) = app_event_tx.send(AppEvent::AddPeer(Peer::new(
                            id.to_string(),
                            Some(display_name.clone()),
                            PeerState::Connecting(ip_address.clone()),
                            denoise,
                            volume,
                        ))) {
                            log::debug!("Failed to send app event: {:?}", e);
                        }
                    }
                }
            }
            _ => loop {
                tokio::time::sleep(std::time::Duration::from_secs(10000)).await;
            },
        }
    }
}
