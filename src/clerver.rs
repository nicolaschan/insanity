use std::sync::Arc;

use cpal::traits::{HostTrait, StreamTrait};

use opus::{Application, Channels, Decoder, Encoder};
use serde::{Deserialize, Serialize};
use tokio::join;
use veq::veq::VeqSessionAlias;

use crate::{
    client::{get_output_config, setup_output_stream},
    processor::{AudioChunk, AudioFormat, AudioProcessor, AUDIO_CHUNK_SIZE},
    protocol::ProtocolMessage,
    server::{make_audio_receiver, AudioReceiver}, resampler::ResampledAudioReceiver,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct AudioFrame(u128, Vec<u8>);

// A clerver is a CLient + sERVER.

pub async fn run_sender<R: AudioReceiver + Send + 'static>(
    mut conn: VeqSessionAlias,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    let audio_receiver = make_receiver();
    let sample_rate = audio_receiver.sample_rate();
    let channels_count = audio_receiver.channels();
    let channels = u16_to_channels(channels_count);

    println!(
        "sending sample_rate: {:?}, channels: {:?}",
        sample_rate, channels
    );
    let mut audio_receiver = ResampledAudioReceiver::new(audio_receiver, 48000);
    let mut encoder = Encoder::new(48000, channels, Application::Audio).unwrap();
    let mut sequence_number = 0;

      loop {
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
        conn.send(buf).await.unwrap();
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

pub async fn run_receiver(mut conn: VeqSessionAlias, enable_denoise: bool) {
    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let processor = Arc::new(AudioProcessor::new(enable_denoise));
    let processor_clone = processor.clone();
    let (sample_format, config) = get_output_config(&output_device);
    let config_clone = config.clone();
    let mut output_stream_wrapper = send_safe::SendWrapperThread::new(move || {
        setup_output_stream(sample_format, config_clone, output_device, processor_clone)
    });
    output_stream_wrapper
        .execute(|output_stream| {
            output_stream.play().unwrap();
        })
        .unwrap();

    println!(
        "receiving sample_rate: {:?}, channels: {:?}",
        config.sample_rate.0,
        u16_to_channels(config.channels)
    );
    let mut decoder = Decoder::new(config.sample_rate.0, u16_to_channels(config.channels)).unwrap();

    while let Ok(mut packet) = conn.recv().await {
        if let Ok(message) = ProtocolMessage::read_from_stream(&mut packet).await {
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
                    processor.handle_incoming(chunk);
                }
                ProtocolMessage::IdentityDeclaration(_) => {}
                ProtocolMessage::PeerDiscovery(_) => {}
            }
        }
    }
}

pub async fn start_clerver(conn: VeqSessionAlias, enable_denoise: bool) {
    let conn_clone = conn.clone();
    let sender = tokio::task::spawn(async move {
        run_sender(conn_clone, make_audio_receiver).await;
    });
    let receiver = run_receiver(conn, enable_denoise);
    let (r, _) = join!(sender, receiver);
    r.unwrap();
}
