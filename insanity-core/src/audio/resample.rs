use crate::audio::chunk::AudioChunk;

pub trait Resampler: Send {
    fn resample(&mut self, chunk: &AudioChunk) -> AudioChunk;
}
