use std::{sync::Arc, thread};

use cpal::traits::{HostTrait, StreamTrait};

use veq::veq::VeqSessionAlias;

use crate::{
    client::setup_output_stream,
    processor::{AudioChunk, AudioFormat, AudioProcessor, AUDIO_CHUNK_SIZE},
    protocol::ProtocolMessage,
    server::AudioReceiver,
};

// A clerver is a CLient + sERVER.

pub async fn run_sender<R: AudioReceiver + Send + 'static>(
    mut conn: VeqSessionAlias,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    let mut audio_receiver = make_receiver();
    let receiver = audio_receiver.receiver();
    let mut sequence_number = 0;

    loop {
        let data: Vec<f32> = receiver.iter().take(AUDIO_CHUNK_SIZE * 2).collect();
        let format = AudioFormat::new(2, 48000);
        let audio_chunk = AudioChunk::new(sequence_number, format, data);
        let mut buf = Vec::new();
        let write_result = audio_chunk.write_to_stream(&mut buf).await;
        conn.send(buf).await.unwrap();
        sequence_number = match write_result {
            Ok(_) => sequence_number + 1,
            Err(_) => {
                break;
            }
        }
    }
}

pub async fn run_receiver(mut conn: VeqSessionAlias, enable_denoise: bool) {
    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let processor = Arc::new(AudioProcessor::new(enable_denoise));
    let processor_clone = processor.clone();
    let mut output_stream_wrapper = send_safe::SendWrapperThread::new(move || {
        setup_output_stream(output_device, processor_clone)
    });
    output_stream_wrapper
        .execute(|output_stream| {
            output_stream.play().unwrap();
        })
        .unwrap();

    while let Ok(mut packet) = conn.recv().await {
        if let Ok(message) = ProtocolMessage::read_from_stream(&mut packet).await {
            match message {
                ProtocolMessage::AudioChunk(chunk) => {
                    processor.handle_incoming(chunk);
                }
                ProtocolMessage::IdentityDeclaration(_) => {}
                ProtocolMessage::PeerDiscovery(_) => {}
            }
        }
    }
}

#[tokio::main]
async fn run_sender_sync<R: AudioReceiver + Send + 'static>(
    conn: VeqSessionAlias,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    run_sender(conn, make_receiver).await;
}

pub async fn start_clerver<R: AudioReceiver + Send + 'static>(
    conn: VeqSessionAlias,
    enable_denoise: bool,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    let conn_clone = conn.clone();
    thread::spawn(move || run_sender_sync(conn_clone, make_receiver));
    let receiver = run_receiver(conn, enable_denoise);
    receiver.await;
}
