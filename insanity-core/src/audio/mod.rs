pub mod capture;
pub mod chunk;
pub mod codec;
pub mod denoiser;
pub mod device;
pub mod jitter;
pub mod mixer;
pub mod sample;
pub mod sample_ops;
pub mod transform;

use serde::{Deserialize, Serialize};

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
