use std::collections::VecDeque;
use std::convert::TryInto;
use std::io::{Read, Write};
use std::net::TcpStream;

use cpal::Sample;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use serde::{Deserialize, Serialize};
use nnnoiseless::DenoiseState;

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
            let mut encoder = ZlibEncoder::new(&mut encoded, Compression::default());
            if std::io::copy(&mut &serialized[..], &mut encoder).is_ok() {}
        }
        let encoded_length: u64 = encoded.len().try_into().unwrap();
        if stream.write(&encoded_length.to_le_bytes()).is_ok() {}
        if stream.write(&encoded).is_ok() {}
    }
    pub fn read_from_stream(mut stream: &TcpStream) -> Result<AudioChunk, std::io::Error> {
        let mut length_buffer = [0; 8];
        stream.read_exact(&mut length_buffer)?;
        let length = u64::from_le_bytes(length_buffer);
        let mut compressed_data_buffer = vec![0; length as usize];
        stream.read_exact(&mut compressed_data_buffer)?;
        let mut data_buffer = Vec::new();
        {
            let mut encoder = ZlibDecoder::new(&compressed_data_buffer[..]);
            if std::io::copy(&mut encoder, &mut data_buffer).is_ok() {}
        }
        Ok(
            bincode::deserialize(&data_buffer[..])
                .expect("Protocol violation: invalid audio chunk"),
        )
    }
}

pub struct AudioProcessor<'a> {
    enable_denoise: bool,
    denoise1: Box<DenoiseState<'a>>,
    denoise2: Box<DenoiseState<'a>>,
    buffer: VecDeque<f32>,
}

impl AudioProcessor<'_> {
    pub fn new(enable_denoise: bool) -> Self {
        AudioProcessor {
            enable_denoise,
            denoise1: DenoiseState::from_model(nnnoiseless::RnnModel::default()),
            denoise2: DenoiseState::from_model(nnnoiseless::RnnModel::default()),
            buffer: VecDeque::new(),
        }
    }

    pub fn handle_incoming(&mut self, chunk: AudioChunk) {
        if self.enable_denoise {
            for audio_chunk in chunk.audio_data.chunks_exact(2 * DenoiseState::FRAME_SIZE) {
                let mut denoised_buffer1 = [0.0; DenoiseState::FRAME_SIZE];
                let mut denoised_buffer2 = [0.0; DenoiseState::FRAME_SIZE];
                let mut chunk1 = [0.0; DenoiseState::FRAME_SIZE];
                let mut chunk2 = [0.0; DenoiseState::FRAME_SIZE];
                for (i, val) in audio_chunk.iter().enumerate() {
                    if i % 2 == 0 {
                        chunk1[i/2] = *val * 32767.0;
                    } else {
                        chunk2[i/2] = *val * 32767.0;
                    }
                }

                self.denoise1.process_frame(&mut denoised_buffer1[..], &chunk1);
                self.denoise2.process_frame(&mut denoised_buffer2[..], &chunk2);

                for (val1, val2) in denoised_buffer1.iter().zip(denoised_buffer2.iter()) {
                    self.buffer.push_back(*val1 / 32767.0);
                    self.buffer.push_back(*val2 / 32767.0);
                }
            }
        } else {
            for val in chunk.audio_data {
                self.buffer.push_back(val);
            }
        }
        // Chunks w/ seq num N than the newest chunk should be discarded.
        // todo: replace 10 with N when decided.
        // If sample rate is 48000 and chunk size is 4800, then 10 will keep us within a second
        while self.buffer.len() > 9600 {
            self.buffer.pop_front();
        }
    }

    pub fn fill_buffer<T: Sample>(&mut self, to_fill: &mut [T]) {
        for val in to_fill.iter_mut(){
            let sample = match self.buffer.pop_front() {
                None => {
                    continue// cry b/c there's no packets
                }
                Some(sample) => Sample::from(&sample)
            };
            *val = sample;
        }
    }
}
