use std::sync::Arc;

use futures_util::StreamExt;
use cpal::traits::{HostTrait, StreamTrait};
use futures_util::future::join;
use quinn::{Connection, IncomingUniStreams, NewConnection};

use crate::{client::setup_output_stream, processor::{AUDIO_CHUNK_SIZE, AudioChunk, AudioFormat, AudioProcessor}, server::AudioReceiver};

// A clerver is a CLient + sERVER.

pub async fn run_sender<R: AudioReceiver + Send + 'static>(conn: Connection, make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static) {
    let mut audio_receiver = make_receiver();
    let receiver = audio_receiver.receiver();
    let mut sequence_number = 0;

    // println!("{:?}", audio_receiver);
    while let Ok(mut send) = conn.open_uni().await {
        let data = receiver.iter().take(AUDIO_CHUNK_SIZE * 2).collect();
        let format = AudioFormat::new(2, 48000);
        let audio_chunk = AudioChunk::new(sequence_number, format, data);
        if audio_chunk.write_to_stream(&mut send).await.is_err() {
            break;
        }
        sequence_number += 1;
        if send.finish().await.is_ok() {}
    }
}

pub async fn run_receiver(mut uni_streams: IncomingUniStreams, enable_denoise: bool) {
    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let processor = Arc::new(AudioProcessor::new(enable_denoise));
    let processor_clone = processor.clone();
    let mut output_stream_wrapper = send_safe::SendWrapperThread::new(move || setup_output_stream(output_device, processor_clone));
    output_stream_wrapper.execute(|output_stream| {
        output_stream.play().unwrap();
        (Some(output_stream), ())
    }).unwrap();

    while let Ok(mut recv) = uni_streams.next().await.unwrap() {
        let chunk = AudioChunk::read_from_stream(&mut recv).await;
        match chunk {
            Ok(chunk) => {
                processor.handle_incoming(chunk);
            },
            Err(_) => {
                break;
            }
        }
    }
}

pub async fn start_clerver<R: AudioReceiver + Send + 'static>(
    conn: NewConnection,
    enable_denoise: bool,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static) {

    let sender = run_sender(conn.connection, make_receiver);
    let receiver = run_receiver(conn.uni_streams, enable_denoise);
    join(sender, receiver).await;
}