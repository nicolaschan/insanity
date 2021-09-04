use std::{net::TcpStream, sync::{Arc, Mutex}, thread};

use cpal::traits::{HostTrait, StreamTrait};

use crate::{client::setup_output_stream, processor::{AudioChunk, AudioFormat, AudioProcessor}, server::AudioReceiver};

// A clerver is a CLient + sERVER.

pub fn start_clerver<R: AudioReceiver + 'static>(
    mut stream: TcpStream,
    enable_denoise: bool,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static) {


    let mut stream_clone = stream.try_clone().unwrap();
    thread::spawn(move || {
        let mut audio_receiver = make_receiver();
        let receiver = audio_receiver.receiver();
        let mut sequence_number = 0;
        loop {
            let data = receiver.iter().take(480).collect();
            let format = AudioFormat::new(0, 0);
            let audio_chunk = AudioChunk::new(sequence_number, format, data);
            if audio_chunk.write_to_stream(&mut stream_clone).is_err() {
                break;
            }
            sequence_number += 1;
        }
    });

    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let processor = Arc::new(AudioProcessor::new(enable_denoise));
    let output_stream = setup_output_stream(output_device, processor.clone());
    output_stream.play().unwrap();
    loop {
        let chunk = AudioChunk::read_from_stream(&mut stream);
        if let Ok(chunk) = chunk {
            processor.handle_incoming(chunk);
        } else {
            break;
        }
    }
}