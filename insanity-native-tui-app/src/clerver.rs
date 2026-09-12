use std::future::Future;
use std::sync::Arc;

use insanity_core::audio::codec::EncodedChunk;
use insanity_core::audio::transform::MetricsState;
use insanity_tui_adapter::AppEvent;
use tokio::sync::{broadcast, mpsc};
use veq::veq::VeqSessionAlias;

use crate::{audio::AudioInputHub, protocol::ProtocolMessage};

// A clerver is a CLient + sERVER.

async fn run_audio_sender(mut conn: VeqSessionAlias, hub: Arc<AudioInputHub>) {
    let mut rx = hub.subscribe();

    loop {
        // Muted and lagged chunks both surface as a seq jump on next recv.
        let frame = match rx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
        };

        let mut buf = Vec::new();
        let protocol_message = ProtocolMessage::Encoded(frame);
        if protocol_message.write_to_stream(&mut buf).await.is_err() {
            break;
        }
        if conn.send(buf).await.is_err() {
            break;
        }
    }
}

async fn run_peer_message_sender(
    mut conn: VeqSessionAlias,
    mut peer_message_receiver: broadcast::Receiver<ProtocolMessage>,
) {
    while let Ok(message) = peer_message_receiver.recv().await {
        let mut buf = Vec::new();
        if message.write_to_stream(&mut buf).await.is_ok() && conn.send(buf).await.is_err() {
            break;
        }
    }
}

/// Loudness reports the previous chunk (~10ms lag): push only queues the frame.
async fn run_receiver<P, F>(
    mut conn: VeqSessionAlias,
    mut push: P,
    loudness: Arc<MetricsState>,
    peer_id: String,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
) where
    P: FnMut(EncodedChunk) -> F,
    F: Future<Output = bool> + Send,
{
    while let Ok(packet) = conn.recv().await {
        if let Ok(message) = ProtocolMessage::read_from_stream(&mut &packet[..]).await {
            match message {
                ProtocolMessage::Encoded(frame) => {
                    if push(frame).await
                        && let Some(sender) = &app_event_sender
                    {
                        let level = loudness.loudness();
                        let _ = sender.send(AppEvent::Loudness(peer_id.clone(), level));
                    }
                }
                ProtocolMessage::IdentityDeclaration(_) => {}
                ProtocolMessage::PeerDiscovery(_) => {}
                ProtocolMessage::ChatMessage(chat_message) => {
                    if let Some(app_event_sender) = &app_event_sender {
                        let _ = app_event_sender
                            .send(AppEvent::NewMessage(peer_id.clone(), chat_message));
                    }
                }
            }
        }
    }
}

pub async fn run_clerver<P, F>(
    conn: VeqSessionAlias,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    hub: Arc<AudioInputHub>,
    push: P,
    loudness: Arc<MetricsState>,
    peer_id: String,
    peer_message_receiver: broadcast::Receiver<ProtocolMessage>,
) where
    P: FnMut(EncodedChunk) -> F,
    F: Future<Output = bool> + Send,
{
    tokio::select! {
        _ = run_audio_sender(
            conn.clone(),
            hub,
        ) => {
            log::debug!("Audio sender for {peer_id} ended early.");
        },
        _ = run_receiver(
            conn.clone(),
            push,
            loudness,
            peer_id.clone(),
            app_event_sender,
        ) => {
            log::debug!("Receiver for {peer_id} ended early.");
        },
        _ = run_peer_message_sender(
            conn,
            peer_message_receiver,
        ) => {
            log::debug!("Peer message sender for {peer_id} ended early.");
        },
    }
}
