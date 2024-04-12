use std::sync::{atomic::AtomicBool, Arc, Mutex};

use cpal::traits::{HostTrait, StreamTrait};

use insanity_tui::AppEvent;
use opus::{Application, Channels, Decoder, Encoder};
use serde::{Deserialize, Serialize};
use tokio::{
    join,
    sync::{
        broadcast::{self, Receiver},
        mpsc,
    },
};
use veq::veq::VeqSessionAlias;

use crate::{
    client::{get_output_config, setup_output_stream},
    processor::{AudioChunk, AudioFormat, AudioProcessor, AUDIO_CHUNK_SIZE},
    protocol::ProtocolMessage,
    resampler::ResampledAudioReceiver,
    server::{make_audio_receiver, AudioReceiver},
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioFrame(u128, Vec<u8>);

// A clerver is a CLient + sERVER.

async fn run_audio_sender<R: AudioReceiver + Send + 'static>(
    mut conn: VeqSessionAlias,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
    mut shutdown: Receiver<()>,
    _termination_sender: mpsc::Sender<()>,
) {
    let audio_receiver = make_receiver();
    let _sample_rate = audio_receiver.sample_rate();
    let channels_count = audio_receiver.channels();
    let channels = u16_to_channels(channels_count);

    let mut audio_receiver = ResampledAudioReceiver::new(audio_receiver, 48000);
    let mut encoder = Encoder::new(48000, channels, Application::Audio).unwrap();
    let mut sequence_number = 0;

    loop {
        if shutdown.try_recv().is_ok() {
            log::debug!("Shutdown received in run_sender");
            return;
        }
        let mut samples = Vec::new();
        let sample_count = AUDIO_CHUNK_SIZE * channels_count as usize;
        for _ in 0..sample_count {
            samples.push(audio_receiver.next().await);
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
    mut shutdown: Receiver<()>,
    _termination_sender: mpsc::Sender<()>,
) {
    while let Ok(message) = tokio::select! {
        message = peer_message_receiver.recv() => message,
        _ = shutdown.recv() => {
            log::debug!("Shutdown received in run_peer_message_sender");
            return;
        },
    } {
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
    mut shutdown: Receiver<()>,
    _termination_sender: mpsc::Sender<()>,
) {
    let id = id.to_string();
    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let processor = Arc::new(AudioProcessor::new(enable_denoise, volume));
    let processor_clone = processor.clone();
    let (sample_format, config) = get_output_config(&output_device);
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

    // println!(
    //     "receiving sample_rate: {:?}, channels: {:?}",
    //     config.sample_rate.0,
    //     u16_to_channels(config.channels)
    // );
    let mut decoder = Decoder::new(config.sample_rate.0, u16_to_channels(config.channels)).unwrap();

    while let Ok(packet) = tokio::select! {
        res = conn.recv() => res,
        _ = shutdown.recv() => {
            log::debug!("Shutdown received in run_receiver");
            return;
        },
    } {
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
                    // let audio_data = bincode::deserialize(&packet).unwrap();
                    let audio_format = AudioFormat::new(config.channels, config.sample_rate.0);
                    let chunk = AudioChunk::new(frame.0, audio_format, buf);
                    // let chunk = AudioChunk::new(frame.0, audio_format, audio_data);
                    let processor_clone = processor.clone();
                    processor_clone.handle_incoming(chunk);
                }
                ProtocolMessage::IdentityDeclaration(_) => {}
                ProtocolMessage::PeerDiscovery(_) => {}
                ProtocolMessage::ChatMessage(chat_message) => {
                    if let &Some(ref app_event_sender) = &app_event_sender {
                        app_event_sender
                            .send(AppEvent::NewMessage(id.clone(), chat_message))
                            .unwrap();
                    }
                }
            }
        }
    }
}

pub async fn start_clerver(
    conn: VeqSessionAlias,
    app_event_sender: Option<mpsc::UnboundedSender<AppEvent>>,
    peer_message_receiver: broadcast::Receiver<ProtocolMessage>,
    enable_denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    id: uuid::Uuid,
    mut shutdown: Receiver<()>,
) {
    let (shutdown_tx, shutdown_rx) = broadcast::channel(10);

    let (audio_sender_termination_tx, mut audio_sender_termination_rx) = mpsc::channel(10);
    let audio_sender = {
        let conn = conn.clone();
        tokio::spawn(async move {
            run_audio_sender(
                conn,
                make_audio_receiver,
                shutdown_rx,
                audio_sender_termination_tx,
            )
            .await;
        })
    };

    let (peer_message_sender_termination_tx, mut peer_message_sender_termination_rx) =
        mpsc::channel(10);
    let peer_message_sender = {
        let conn = conn.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            run_peer_message_sender(
                conn,
                peer_message_receiver,
                shutdown_rx,
                peer_message_sender_termination_tx,
            )
            .await;
        })
    };

    let (receiver_termination_tx, mut receiver_termination_rx) = mpsc::channel(10);
    let receiver = {
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::task::spawn(async move {
            run_receiver(
                conn,
                app_event_sender,
                enable_denoise,
                volume,
                id,
                shutdown_rx,
                receiver_termination_tx,
            )
            .await;
        })
    };

    tokio::select! {
        _ = audio_sender_termination_rx.recv() => {
            log::debug!("Audio Sender termination received");
        },
        _ = peer_message_sender_termination_rx.recv() => {
            log::debug!("Peer message sender termination received")
        },
        _ = receiver_termination_rx.recv() => {
            log::debug!("Receiver termination received");
        },
        _ = shutdown.recv() => {
            log::debug!("Shutdown received in start_clerver");
        }
    }
    if shutdown_tx.send(()).is_ok() {
        log::debug!("Shutdown sent to broadcast channel");
    } else {
        log::debug!("Failed to send shutdown to broadcast channel");
    }
    let (s, p, r) = join!(audio_sender, peer_message_sender, receiver);
    s.unwrap();
    p.unwrap();
    r.unwrap();
}
