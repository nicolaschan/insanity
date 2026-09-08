use serde::{Deserialize, Serialize};

use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodedChunk {
    pub sequence_number: u128,
    pub codec: AudioCodec,
    pub payload: Vec<u8>,
    pub format: AudioFormat,
}

pub trait AudioEncoder: Send {
    fn encode(&mut self, chunk: &AudioChunk) -> Option<EncodedChunk>;
}

pub trait AudioDecoder: Send {
    fn decode(&mut self, frame: &EncodedChunk) -> Option<AudioChunk>;
}
