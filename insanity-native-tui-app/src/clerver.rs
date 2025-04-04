use std::sync::atomic::Ordering;
use std::sync::{atomic::AtomicBool, Arc, Mutex};

use cpal::traits::{HostTrait, StreamTrait};

use insanity_core::audio_source::AudioSource;
use insanity_tui_adapter::AppEvent;
use opus::{Application, Channels, Decoder, Encoder};
use rubato_audio_source::ResampledAudioSource;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use veq::veq::VeqSessionAlias;

use crate::{
    client::{get_output_config, setup_output_stream},
    processor::{AudioChunk, AudioFormat, AudioProcessor, AUDIO_CHUNK_SIZE},
    protocol::ProtocolMessage,
    server::make_audio_receiver,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioFrame(u128, Vec<u8>);

// A clerver is a CLient + sERVER.

async fn run_audio_sender<R: AudioSource + Send + Sync + 'static>(
    mut conn: VeqSessionAlias,
    sender_is_muted: Arc<AtomicBool>,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    let audio_receiver = make_receiver();
    let _sample_rate = audio_receiver.sample_rate();
    let channels_count = audio_receiver.channels();
    let channels = u16_to_channels(channels_count);

    let mut audio_receiver = ResampledAudioSource::new(audio_receiver, 48000, AUDIO_CHUNK_SIZE);
    let mut encoder = Encoder::new(48000, channels, Application::Audio).unwrap();
    let mut sequence_number = 0;

    loop {
        let mut samples = Vec::new();
        let sample_count = AUDIO_CHUNK_SIZE * channels_count as usize;
        for _ in 0..sample_count {
            // .unwrap() is ok here because this type of audio receiver is "infinite"
            // and will delay on await instead of return None
            samples.push(audio_receiver.next().await.unwrap());
        }

        if sender_is_muted.load(Ordering::Relaxed) {
            continue; // skip encoding and sending
        }

        // let samples: Vec<f32> = receiver.iter().take(AUDIO_CHUNK_SIZE * 2).collect();
        let opus_frame = encoder.encode_vec_float(&samples[..], 65535).unwrap();
        // let opus_frame = bincode::serialize(&samples).unwrap();
        let frame = AudioFrame(sequence_number, opus_frame);

        let mut buf = Vec::new();
        let protocol_message = ProtocolMessage::AudioFrame(frame);
        let write_result = protocol_message.write_to_stream(&mut buf).await;
        if conn.send(buf).await.is_err() {
            break;
        }
        sequence_number = match write_result {
            Ok(_) => sequence_number + 1,
            Err(_) => {
                break;
            }
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
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    enable_denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    id: uuid::Uuid,
) {
    let id = id.to_string();
    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let (sample_format, config) = get_output_config(&output_device);
    let processor = Arc::new(AudioProcessor::new(
        enable_denoise,
        volume,
        config.sample_rate,
        app_event_sender.clone(),
        id.clone(),
    ));
    let processor_clone = processor.clone();
    let config_clone = config.clone();
    let mut output_stream_wrapper = send_safe::SendWrapperThread::new(move || {
        setup_output_stream(
            &sample_format,
            &config_clone,
            &output_device,
            processor_clone,
        )
    });
    output_stream_wrapper
        .execute(|output_stream| {
            output_stream.play().unwrap();
        })
        .unwrap();

    let mut decoder = Decoder::new(48000, u16_to_channels(config.channels)).unwrap();

    while let Ok(packet) = conn.recv().await {
        if let Ok(message) = ProtocolMessage::read_from_stream(&mut &packet[..]).await {
            match message {
                ProtocolMessage::AudioFrame(frame) => {
                    let packet = frame.1;
                    let len =
                        decoder.get_nb_samples(&packet[..]).unwrap() * (config.channels as usize);
                    let mut buf = vec![0f32; len];
                    decoder
                        .decode_float(&packet[..], &mut buf[..], false)
                        .unwrap();
                    let audio_format = AudioFormat::new(config.channels, config.sample_rate.0);
                    let chunk = AudioChunk::new(frame.0, audio_format, buf);
                    processor.handle_incoming(chunk);
                }
                ProtocolMessage::IdentityDeclaration(_) => {}
                ProtocolMessage::PeerDiscovery(_) => {}
                ProtocolMessage::ChatMessage(chat_message) => {
                    if let Some(app_event_sender) = &app_event_sender {
                        app_event_sender
                            .send(AppEvent::NewMessage(id.clone(), chat_message))
                            .unwrap();
                    }
                }
            }
        }
    }
}

pub async fn run_clerver(
    conn: VeqSessionAlias,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    sender_is_muted: Arc<AtomicBool>,
    peer_message_receiver: broadcast::Receiver<ProtocolMessage>,
    enable_denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    id: uuid::Uuid,
) {
    tokio::select! {
        _ = run_audio_sender(
            conn.clone(),
            sender_is_muted,
            make_audio_receiver,
        ) => {
            log::debug!("Audio sender for {id} ended early.");
        },
        _ = run_receiver(
            conn.clone(),
            app_event_sender,
            enable_denoise,
            volume,
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
