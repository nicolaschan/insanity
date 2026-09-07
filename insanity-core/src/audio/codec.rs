use serde::{Deserialize, Serialize};

use crate::audio::chunk::AudioChunk;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFrame {
    pub sequence_number: u128,
    pub payload: Vec<u8>,
}

pub trait AudioEncoder: Send {
    fn encode(&mut self, chunk: &AudioChunk) -> Option<AudioFrame>;
}

pub trait AudioDecoder: Send {
    fn decode(&mut self, frame: &AudioFrame) -> Option<AudioChunk>;
}
