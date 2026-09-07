use crate::audio::AudioFormat;
use serde::{Deserialize, Serialize};
use std::future::Future;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub sequence_number: u128,
    pub audio_data: Vec<f32>,
}

impl AudioChunk {
    pub fn new(sequence_number: u128, audio_data: Vec<f32>) -> AudioChunk {
        AudioChunk {
            sequence_number,
            audio_data,
        }
    }
}

pub trait ChunkSource {
    fn format(&self) -> AudioFormat;
    fn next_chunk(&mut self) -> impl Future<Output = Option<AudioChunk>> + Send;
}

pub trait ChunkSink {
    fn format(&self) -> AudioFormat;
    fn push_chunk(&mut self, chunk: AudioChunk);
}
