use std::{net::TcpStream, sync::{Arc, Mutex}, thread, time::Duration};

use futures_util::StreamExt;
use cpal::traits::{HostTrait, StreamTrait};
use futures_util::future::join;
use quinn::{Connection, ConnectionError, IncomingBiStreams, IncomingUniStreams, NewConnection, RecvStream, SendStream, crypto::rustls::TlsSession};
use tokio::{io::{AsyncRead, AsyncReadExt, AsyncWriteExt}};

use crate::{client::setup_output_stream, processor::{AUDIO_CHUNK_SIZE, AudioChunk, AudioFormat, AudioProcessor}, server::{AudioReceiver, CpalStreamReceiver}};

// A clerver is a CLient + sERVER.

pub async fn run_sender<R: AudioReceiver + Send + 'static>(mut conn: Connection, make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static) {
    let mut audio_receiver = make_receiver();
    let receiver = audio_receiver.receiver();
    let mut sequence_number = 0;

    loop {
        let mut send = conn.open_uni().await.unwrap();
        let data = receiver.iter().take(AUDIO_CHUNK_SIZE * 2).collect();
        let format = AudioFormat::new(2, 48000);
        let audio_chunk = AudioChunk::new(sequence_number, format, data);
        if audio_chunk.write_to_stream(&mut send).await.is_err() {
            break;
        }
        sequence_number += 1;
        send.finish().await;
    }
}

pub async fn run_receiver(mut uni_streams: IncomingUniStreams, enable_denoise: bool) {
    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let processor = Arc::new(AudioProcessor::new(enable_denoise));
    let output_stream = Arc::new(Mutex::new(setup_output_stream(output_device, processor.clone())));
    output_stream.lock().unwrap().play().unwrap();

    while let Ok(mut recv) = uni_streams.next().await.unwrap() {
        let chunk = AudioChunk::read_from_stream(&mut recv).await;
        match chunk {
            Ok(chunk) => {
                processor.handle_incoming(chunk);
            },
            Err(e) => {
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