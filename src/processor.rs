extern crate test;

use std::collections::VecDeque;
use std::convert::TryInto;
use std::io::{Read, Write};
use std::sync::Mutex;

use cpal::Sample;
use nnnoiseless::DenoiseState;
use serde::{Deserialize, Serialize};

use crate::realtime_buffer::RealTimeBuffer;

pub const AUDIO_CHUNK_SIZE: usize = 480;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
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

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct AudioChunk {
    pub sequence_number: u128,
    pub audio_data: Vec<f32>,
    pub audio_format: AudioFormat,
}

impl AudioChunk {
    pub fn new(sequence_number: u128, audio_format: AudioFormat, audio_data: Vec<f32>) -> AudioChunk {
        AudioChunk {
            sequence_number,
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
    pub fn write_to_stream<T: Write>(&self, stream: &mut T) -> Result<(), std::io::Error> {
        let serialized = bincode::serialize(self).expect("Could not serialize AudioChunk");
        let mut encoded: Vec<u8> = Vec::new();
        if zstd::stream::copy_encode(&serialized[..], &mut encoded, 1).is_ok() {}
        let encoded_length: u64 = encoded.len().try_into().unwrap();
        // println!("compression ratio {}", (serialized.len() as f64) / (encoded_length as f64));
        stream.write_all(&encoded_length.to_le_bytes())?;
        stream.write_all(&encoded)?;
        Ok(())
    }
    pub fn read_from_stream<T: Read>(stream: &mut T) -> Result<AudioChunk, std::io::Error> {
        let mut length_buffer = [0; 8];
        stream.read_exact(&mut length_buffer)?;
        let length = u64::from_le_bytes(length_buffer);
        let mut compressed_data_buffer = vec![0; length as usize];
        stream.read_exact(&mut compressed_data_buffer)?;
        let mut data_buffer = Vec::new();
        if zstd::stream::copy_decode(&compressed_data_buffer[..], &mut data_buffer).is_ok() {}
        Ok(
            bincode::deserialize(&data_buffer[..])
                .expect("Protocol violation: invalid audio chunk"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test::Bencher;

    #[test]
    fn read_write_protocol_works() {
        let mut output: Vec<u8> = Vec::new();
        let chunk = AudioChunk::new(0, AudioFormat::new(0, 0), [0.5; 4800].to_vec());
        chunk.write_to_stream(&mut output).unwrap();
        let received = AudioChunk::read_from_stream(&mut &output[..]).unwrap();
        assert_eq!(chunk, received);
    }

    #[bench]
    fn bench_write_to_stream(b: &mut Bencher) {
        let mut output: Vec<u8> = Vec::new();
        let chunk = AudioChunk::new(0, AudioFormat::new(0, 0), [0.0; 4800].to_vec());
        b.iter(|| chunk.write_to_stream(&mut output))
    }

    #[bench]
    fn bench_read_from_stream(b: &mut Bencher) {
        let mut output: Vec<u8> = Vec::new();
        let chunk = AudioChunk::new(0, AudioFormat::new(0, 0), [0.0; 4800].to_vec());
        chunk.write_to_stream(&mut output).unwrap();
        b.iter(|| AudioChunk::read_from_stream(&mut &output[..]).unwrap())
    }

    #[bench]
    fn bench_processor_handle_incoming(b: &mut Bencher) {
        let mut processor = AudioProcessor::new(false);
        b.iter(move || {
            let chunk = AudioChunk::new(0, AudioFormat::new(0, 0), [0.0; 4800].to_vec());
            processor.handle_incoming(chunk)
        });
    }

    #[bench]
    fn bench_processor_handle_incoming_denoised(b: &mut Bencher) {
        let mut processor = AudioProcessor::new(true);
        b.iter(|| {
            let chunk = AudioChunk::new(0, AudioFormat::new(0, 0), [0.0; 4800].to_vec());
            processor.handle_incoming(chunk)
        });
    }
}

pub struct AudioProcessor<'a> {
    enable_denoise: bool,
    denoise1: Box<DenoiseState<'a>>,
    denoise2: Box<DenoiseState<'a>>,
    audio_buffer: Mutex<VecDeque<f32>>,
    chunk_buffer: Mutex<RealTimeBuffer<AudioChunk>>,
}

impl AudioProcessor<'_> {
    pub fn new(enable_denoise: bool) -> Self {
        AudioProcessor {
            enable_denoise,
            denoise1: DenoiseState::from_model(nnnoiseless::RnnModel::default()),
            denoise2: DenoiseState::from_model(nnnoiseless::RnnModel::default()),
            chunk_buffer: Mutex::new(RealTimeBuffer::new(20)),
            audio_buffer: Mutex::new(VecDeque::new()),
        }
    }

    pub fn handle_incoming(&self, chunk: AudioChunk) {
        let mut guard = self.chunk_buffer.lock().unwrap();
        guard.set(chunk.sequence_number, chunk);
    }

    pub fn fill_buffer<T: Sample>(&self, to_fill: &mut [T]) {
        let mut audio_buffer_guard = self.audio_buffer.lock().unwrap();
        let mut i = 0;
        while to_fill.len() > audio_buffer_guard.len() {
            let mut guard = self.chunk_buffer.lock().unwrap();
            match guard.next() {
                Some(chunk) => audio_buffer_guard.extend(chunk.audio_data),
                None => {},
            };
            i += 1;
        }
        for val in to_fill.iter_mut() {
            let sample = match audio_buffer_guard.pop_front() {
                None => {
                    Sample::from(&0.0) // cry b/c there's no packets
                }
                Some(sample) => Sample::from(&sample),
            };
            *val = sample;
        }
    }
}
