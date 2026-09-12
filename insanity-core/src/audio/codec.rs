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

pub(crate) struct FormatCache<C, F: FnMut(&AudioFormat) -> Option<C>> {
    codec: Option<C>,
    format: Option<AudioFormat>,
    rebuild: F,
}

impl<C, F: FnMut(&AudioFormat) -> Option<C>> FormatCache<C, F> {
    pub(crate) fn new(rebuild: F) -> Self {
        FormatCache {
            codec: None,
            format: None,
            rebuild,
        }
    }

    pub(crate) fn format(&self) -> Option<&AudioFormat> {
        self.format.as_ref()
    }

    pub(crate) fn ensure_current(&mut self, format: &AudioFormat) -> Option<&mut C> {
        if self.format.as_ref() != Some(format) {
            let codec = (self.rebuild)(format)?;
            self.codec = Some(codec);
            self.format = Some(format.clone());
        }
        self.codec.as_mut()
    }

    pub(crate) fn reset(&mut self) {
        let Some(format) = self.format.clone() else {
            return;
        };
        self.codec = (self.rebuild)(&format);
    }
}
