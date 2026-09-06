use serde::{Deserialize, Serialize};

pub mod denoiser;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct AudioFormat {
    pub channel_count: u16,
    pub sample_rate: u32,
}

impl AudioFormat {
    pub fn new(channel_count: u16, sample_rate: u32) -> AudioFormat {
        AudioFormat {
            channel_count,
            sample_rate,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub sequence_number: u128,
    pub audio_data: Vec<f32>,
    pub audio_format: AudioFormat,
}

impl AudioChunk {
    pub fn new(
        sequence_number: u128,
        audio_format: AudioFormat,
        audio_data: Vec<f32>,
    ) -> AudioChunk {
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
}
