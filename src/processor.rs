// use std::collections::VecDeque;
use serde::{Deserialize, Serialize};

use std::convert::TryInto;
use std::io::Write;
use std::net::TcpStream;

pub const AUDIO_CHUNK_SIZE: usize = 1024;

#[derive(Serialize, Deserialize, Debug)]
pub struct AudioFormat {
    channel_count: u16,
    sample_rate: u32,
}

impl AudioFormat {
    pub fn new(channel_count: u16, sample_rate: u32) -> AudioFormat {
        AudioFormat {
            channel_count,
            sample_rate,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AudioChunk {
    pub sequence_number: u128,
    pub audio_data: Vec<f32>,
    pub audio_format: AudioFormat,
}

impl AudioChunk {
    pub fn new(audio_format: AudioFormat, audio_data: Vec<f32>) -> AudioChunk {
        AudioChunk {
            sequence_number: 0,
            audio_data,
            audio_format,
        }
    }
    pub fn to_format(&self, format: AudioFormat) -> AudioChunk {
        AudioChunk {
            sequence_number: self.sequence_number,
            audio_data: self.audio_data.clone(),
            audio_format: format,
        }
    }
    pub fn write_to_stream(&self, mut stream: &TcpStream) {
        let serialized = bincode::serialize(self).expect("Could not serialize AudioChunk");
        let mut encoded: Vec<u8> = Vec::new();
        {
            let mut encoder = snap::write::FrameEncoder::new(&mut encoded);
            if std::io::copy(&mut &serialized[..], &mut encoder).is_ok() {}
        }
        let encoded_length: u64 = encoded.len().try_into().unwrap();
        if stream.write(&encoded_length.to_le_bytes()).is_ok() {}
        if stream.write(&encoded).is_ok() {}
    }
}

// pub struct AudioProcessor {
//     audio_output_sender: Sender<AudioChunk>,
//     mut buffer: VecDeque<AudioChunk>,
// }

// impl AudioProcessor {
//     pub fn new(audio_output_sender: Sender<f32>) {
//         let audio_processor = AudioProcessor {
//             audio_output_sender,
//             buffer: VecDeque::new(),
//         };
//         thread::spawn(move || {
//             loop {
//                 audio_output_sender.send(buffer.pop_front());
//             }
//         });
//         audio_processor
//     }

//     pub fn handle_incoming(&self, chunk: AudioChunk) {
//         self.buffer.push_back(chunk);
//     }
// }
