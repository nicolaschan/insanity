pub const AUDIO_CHUNK_SIZE: usize = 480;
pub const AUDIO_CHANNELS: u16 = 2;

// Canonical audio types live in insanity-core (moved there by upstream
// "Abstract out denoiser"). Re-export them here so the single-audio-io
// hub/mixer architecture and its tests/tools keep their existing
// `crate::processor::{AudioChunk, AudioFormat, MultiChannelDenoiser}` paths.
pub use insanity_core::audio::{AudioChunk, AudioFormat};
