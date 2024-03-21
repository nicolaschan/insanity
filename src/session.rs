use tokio::sync::{
    broadcast,
    mpsc::{self, Receiver, Sender},
    Mutex,
};
use veq::veq::{VeqSessionAlias, VeqSocket};

use crate::coordinator::AugmentedInfo;

pub struct UpdatablePendingSession {
    socket: VeqSocket,
    terminate_tx: broadcast::Sender<()>,
    session_sender: Sender<(VeqSessionAlias, AugmentedInfo)>,
    session_receiver: Mutex<Receiver<(VeqSessionAlias, AugmentedInfo)>>,
    session: Mutex<Option<(VeqSessionAlias, AugmentedInfo)>>,
    current_info: Mutex<Option<AugmentedInfo>>,
}

impl UpdatablePendingSession {
    pub fn new(socket: VeqSocket) -> UpdatablePendingSession {
        let (terminate_tx, _terminate_rx) = broadcast::channel(1);
        let (session_sender, session_receiver) = mpsc::channel(1);
        UpdatablePendingSession {
            socket,
            terminate_tx,
            session_sender,
            session_receiver: Mutex::new(session_receiver),
            session: Mutex::new(None),
            current_info: Mutex::new(None),
        }
    }

    pub async fn update(&self, id: uuid::Uuid, info: AugmentedInfo) {
        let mut current_info_guard = self.current_info.lock().await;
        if let Some(current_info) = (*current_info_guard).as_ref() {
            if current_info == &info {
                return;
            }
        }
        log::debug!("Got updated info for session {}: {:?}", id, info);
        *current_info_guard = Some(info.clone());

        if let Err(e) = self.terminate_tx.send(()) {
            log::warn!("Error sending terminate signal: {}", e);
        }

        let mut terminate_rx = self.terminate_tx.subscribe();
        let session_sender = self.session_sender.clone();
        let mut socket = self.socket.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = terminate_rx.recv() => {
                    log::info!("Terminating pending session {}", id);
                }
                session = socket.connect(id, info.conn_info.clone()) => {
                    if let Err(e) = session_sender.send((session, info)).await {
                        log::warn!("Error sending session: {}", e);
                    }
                }
            }
        });
    }

    pub async fn session(&self) -> (VeqSessionAlias, AugmentedInfo) {
        let mut session_guard = self.session.lock().await;
        if let Some((session, info)) = session_guard.as_ref() {
            return (session.clone(), info.clone());
        }

        let mut receiver_guard = self.session_receiver.lock().await;
        let (session, info) = receiver_guard.recv().await.unwrap();
        receiver_guard.close();

        *session_guard = Some((session.clone(), info.clone()));

        if let Err(e) = self.terminate_tx.send(()) {
            log::warn!("Error sending terminate signal: {}", e);
        }

        (session, info)
    }
}
