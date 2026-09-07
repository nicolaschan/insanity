use std::sync::Arc;

use insanity_core::audio::codec::AudioFrame;
use insanity_core::audio::{AudioFormat, chunk::AudioChunk};
use insanity_tui_adapter::AppEvent;
use opus::Decoder;
use tokio::sync::{broadcast, mpsc};
use veq::veq::VeqSessionAlias;

use crate::{
    audio::{AudioInputHub, AudioMixer},
    codec_opus::u16_to_channels,
    processor::AUDIO_SAMPLE_RATE,
    protocol::ProtocolMessage,
};

// A clerver is a CLient + sERVER.

pub fn decode_frame_to_chunk(
    decoder: &mut Decoder,
    frame: &AudioFrame,
    channels: u16,
) -> Option<AudioChunk> {
    let Ok(nb) = decoder.get_nb_samples(&frame.payload[..]) else {
        return None;
    };
    let len = nb * (channels as usize);
    let mut buf = vec![0f32; len];
    if decoder
        .decode_float(&frame.payload[..], &mut buf[..], false)
        .is_err()
    {
        return None;
    }
    Some(AudioChunk::new(
        frame.sequence_number,
        AudioFormat::new(channels, AUDIO_SAMPLE_RATE),
        buf,
    ))
}

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
        let protocol_message = ProtocolMessage::AudioFrame(frame);
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

async fn run_receiver(
    mut conn: VeqSessionAlias,
    mixer: Arc<AudioMixer>,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    id: uuid::Uuid,
) {
    let id_str = id.to_string();
    let Ok(mut decoder) = Decoder::new(AUDIO_SAMPLE_RATE, u16_to_channels(mixer.channels())) else {
        log::error!("Failed to create Opus decoder for peer {id}; receiver disabled");
        return;
    };

    while let Ok(packet) = conn.recv().await {
        if let Ok(message) = ProtocolMessage::read_from_stream(&mut &packet[..]).await {
            match message {
                ProtocolMessage::AudioFrame(frame) => {
                    let channels = mixer.channels();
                    let Some(chunk) = decode_frame_to_chunk(&mut decoder, &frame, channels) else {
                        continue;
                    };
                    mixer.handle_incoming(id, chunk);
                }
                ProtocolMessage::IdentityDeclaration(_) => {}
                ProtocolMessage::PeerDiscovery(_) => {}
                ProtocolMessage::ChatMessage(chat_message) => {
                    if let Some(app_event_sender) = &app_event_sender {
                        let _ = app_event_sender
                            .send(AppEvent::NewMessage(id_str.clone(), chat_message));
                    }
                }
            }
        }
    }
    mixer.remove_peer(&id);
}

pub async fn run_clerver(
    conn: VeqSessionAlias,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    hub: Arc<AudioInputHub>,
    mixer: Arc<AudioMixer>,
    peer_message_receiver: broadcast::Receiver<ProtocolMessage>,
    id: uuid::Uuid,
) {
    tokio::select! {
        _ = run_audio_sender(
            conn.clone(),
            hub,
        ) => {
            log::debug!("Audio sender for {id} ended early.");
        },
        _ = run_receiver(
            conn.clone(),
            mixer.clone(),
            app_event_sender,
            id,
        ) => {
            log::debug!("Receiver for {id} ended early.");
        },
        _ = run_peer_message_sender(
            conn,
            peer_message_receiver,
        ) => {
            log::debug!("Peer message sender for {id} ended early.");
        },
    }
    mixer.remove_peer(&id);
}
