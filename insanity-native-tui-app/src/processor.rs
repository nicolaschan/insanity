pub const AUDIO_CHUNK_SIZE: usize = 480;
pub const AUDIO_CHANNELS: u16 = 2;

// Re-exported from insanity-core so `crate::processor::*` paths keep working.
pub use insanity_core::audio::{AudioChunk, AudioFormat};
