use std::{sync::Arc};

use cpal::traits::{HostTrait, StreamTrait};
use futures_util::future::join;
use futures_util::StreamExt;
use quinn::{Connection, IncomingUniStreams, NewConnection};

use crate::{client::setup_output_stream, processor::{AudioChunk, AudioFormat, AudioProcessor, AUDIO_CHUNK_SIZE}, protocol::ProtocolMessage, server::AudioReceiver};

// A clerver is a CLient + sERVER.

pub async fn run_sender<R: AudioReceiver + Send + 'static>(
    conn: Connection,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    let mut audio_receiver = make_receiver();
    let receiver = audio_receiver.receiver();
    let mut sequence_number = 0;

    // println!("{:?}", audio_receiver);
    while let Ok(mut send) = conn.open_uni().await {
        let data = receiver.iter().take(AUDIO_CHUNK_SIZE * 2).collect();
        let format = AudioFormat::new(2, 48000);
        let audio_chunk = AudioChunk::new(sequence_number, format, data);
        let write_result = audio_chunk.write_to_stream(&mut send).await;
        if send.finish().await.is_ok() {}
        sequence_number = match write_result {
            Ok(_) => sequence_number + 1,
            Err(_) => { break; },
        }
    }
}

pub async fn run_receiver(mut uni_streams: IncomingUniStreams, enable_denoise: bool) {
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

    while let Ok(recv) = uni_streams.next().await.unwrap() {
        let protocol_message = ProtocolMessage::read_from_stream(recv).await;
        if protocol_message.is_err() {
            break;
        }
        match protocol_message {
            Ok(message) => {
                match message {
                    ProtocolMessage::AudioChunk(chunk) => { processor.handle_incoming(chunk); },
                    ProtocolMessage::IdentityDeclaration(_) => todo!(),
                    ProtocolMessage::PeerDiscovery(_) => todo!(),
                }
            },
            Err(_) => { break; }
        }
    }
}

pub async fn start_clerver<R: AudioReceiver + Send + 'static>(
    conn: NewConnection,
    enable_denoise: bool,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    let sender = run_sender(conn.connection, make_receiver);
    let receiver = run_receiver(conn.uni_streams, enable_denoise);
    join(sender, receiver).await;
}
