use std::sync::Arc;

use insanity_tui_adapter::AppEvent;
use opus::{Application, Channels, Decoder, Encoder};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use veq::veq::VeqSessionAlias;

use crate::{
    audio::{AudioInputHub, AudioMixer},
    processor::{AudioChunk, AudioFormat},
    protocol::ProtocolMessage,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioFrame(u128, Vec<u8>);

// A clerver is a CLient + sERVER.

pub fn encode_hub_chunk(
    encoder: &mut Encoder,
    sequence_number: u128,
    chunk: &[f32],
) -> Option<AudioFrame> {
    match encoder.encode_vec_float(chunk, 65535) {
        Ok(payload) => Some(AudioFrame(sequence_number, payload)),
        Err(e) => {
            log::warn!("Opus encode failed: {e:?}");
            None
        }
    }
}

pub fn decode_frame_to_chunk(
    decoder: &mut Decoder,
    frame: &AudioFrame,
    channels: u16,
    sample_rate: u32,
) -> Option<AudioChunk> {
    let Ok(nb) = decoder.get_nb_samples(&frame.1[..]) else {
        return None;
    };
    let len = nb * (channels as usize);
    let mut buf = vec![0f32; len];
    if decoder
        .decode_float(&frame.1[..], &mut buf[..], false)
        .is_err()
    {
        return None;
    }
    Some(AudioChunk::new(
        frame.0,
        AudioFormat::new(channels, sample_rate),
        buf,
    ))
}

async fn run_audio_sender(mut conn: VeqSessionAlias, hub: Arc<AudioInputHub>) {
    let channels = u16_to_channels(hub.channels());
    let Ok(mut encoder) = Encoder::new(48000, channels, Application::Audio) else {
        log::error!("Failed to create Opus encoder; audio sender for hub disabled");
        return;
    };
    let mut rx = hub.subscribe();

    loop {
        // Hub seq is wall-clock: muted ticks are not sent (no silence encode)
        // but still advance seq, so the next received chunk jumps honestly.
        // Lagged (slow consumer) also surfaces as a seq jump on next recv.
        let (sequence_number, chunk) = match rx.recv().await {
            Ok(c) => c,
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
        };

        let Some(frame) = encode_hub_chunk(&mut encoder, sequence_number, &chunk[..]) else {
            continue;
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

fn u16_to_channels(n: u16) -> Channels {
    match n {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        _ => Channels::Stereo,
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
    let Ok(mut decoder) = Decoder::new(48000, u16_to_channels(mixer.channels())) else {
        log::error!("Failed to create Opus decoder for peer {id}; receiver disabled");
        return;
    };

    while let Ok(packet) = conn.recv().await {
        if let Ok(message) = ProtocolMessage::read_from_stream(&mut &packet[..]).await {
            match message {
                ProtocolMessage::AudioFrame(frame) => {
                    let Some(chunk) = decode_frame_to_chunk(
                        &mut decoder,
                        &frame,
                        mixer.channels(),
                        mixer.sample_rate(),
                    ) else {
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
            mixer,
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
}
