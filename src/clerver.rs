use std::{io::{Read, Write}, net::TcpStream, sync::{Arc, Mutex}, thread};

use cpal::traits::{HostTrait, StreamTrait};
use crossbeam::channel::{Receiver, unbounded};

use crate::{client::setup_output_stream, processor::{AudioChunk, AudioFormat, AudioProcessor}, server::AudioReceiver};

// A clerver is a CLient + sERVER.

struct ReadableReceiver {
    receiver: Receiver<u8>,
}

impl Read for ReadableReceiver {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut i = 0;
        while let Ok(val) = self.receiver.try_recv() {
            if i >= buf.len() {
                break;
            }
            buf[i] = val;
            i = i + 1;
        }
        Ok(i)
    }
}

impl ReadableReceiver {
    pub fn new(receiver: Receiver<u8>) -> ReadableReceiver {
        ReadableReceiver {
            receiver
        }
    }
}

pub fn start_clerver<R: AudioReceiver + 'static>(
    mut stream: TcpStream,
    enable_denoise: bool,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static) {

    let host = cpal::default_host();
    let output_device = host.default_output_device().unwrap();
    let processor = Arc::new(Mutex::new(AudioProcessor::new(enable_denoise)));
    let output_stream = setup_output_stream(output_device, processor.clone());
    output_stream.play().unwrap();

    let mut stream_clone = stream.try_clone().unwrap();
    thread::spawn(move || {
        let mut audio_receiver = make_receiver();
        let receiver = audio_receiver.receiver();
        loop {
            let data = receiver.iter().take(4800).collect();
            let format = AudioFormat::new(0, 0);
            let audio_chunk = AudioChunk::new(format, data);
            if audio_chunk.write_to_stream(&mut stream_clone).is_err() {
                break;
            }
        }
    });

    loop {
        let chunk = AudioChunk::read_from_stream(&mut stream);
        if let Ok(chunk) = chunk {
            processor.lock().unwrap().handle_incoming(chunk);
        }
    }
}