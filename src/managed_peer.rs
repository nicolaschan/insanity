use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use tokio::sync::broadcast;

use crate::{connection_manager::AugmentedInfo, protocol::ProtocolMessage};

#[derive(Clone)]
pub struct ManagedPeer {
    pub denoise: Arc<AtomicBool>,
    pub volume: Arc<Mutex<usize>>,
    pub connection_info: veq::veq::ConnectionInfo,
    pub display_name: String,
    pub shutdown_tx: broadcast::Sender<()>,
    pub peer_message_sender: broadcast::Sender<ProtocolMessage>,
}

impl ManagedPeer {
    pub fn new(
        denoise: bool,
        volume: usize,
        connection_info: veq::veq::ConnectionInfo,
        display_name: String,
    ) -> ManagedPeer {
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(10);
        let (peer_message_sender, _) = broadcast::channel(10);
        ManagedPeer {
            denoise: Arc::new(AtomicBool::new(denoise)),
            volume: Arc::new(Mutex::new(volume)),
            connection_info,
            display_name,
            shutdown_tx,
            peer_message_sender,
        }
    }

    pub fn info(&self) -> AugmentedInfo {
        AugmentedInfo {
            connection_info: self.connection_info.clone(),
            display_name: self.display_name.clone(),
        }
    }

    pub fn set_denoise(&self, denoise: bool) {
        self.denoise.store(denoise, Ordering::Relaxed);
    }

    pub fn set_volume(&self, volume: usize) {
        let mut volume_guard = self.volume.lock().unwrap();
        *volume_guard = volume;
    }

    pub fn send_message(&self, message: String) -> anyhow::Result<()> {
        let protocol_message = ProtocolMessage::ChatMessage(message);
        if self.peer_message_sender.receiver_count() > 0 {
            self.peer_message_sender.send(protocol_message)?;
        }
        Ok(())
    }

    pub fn enable(&self) {
        // TODO: implement this.
        log::warn!("Re-enabling peer not implemented.");
    }

    pub fn disable(&self) -> anyhow::Result<()> {
        if self.shutdown_tx.send(()).is_ok() {
            log::info!("Disabled peer");
            Ok(())
        } else {
            Err(anyhow::anyhow!("Failed to disable peer"))
        }
    }
}
