use crate::audio::chunk::AudioChunk;

pub trait Denoiser: Send {
    const FRAME_SIZE: usize;
    fn init() -> Self;
    fn process_frame(&mut self, output: &mut [f32], input: &[f32]);
}

pub struct MultiChannelDenoiser<T: Denoiser> {
    channels: u16,
    denoisers: Vec<T>,
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
}

impl<T: Denoiser> Default for MultiChannelDenoiser<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Denoiser> MultiChannelDenoiser<T> {
    pub fn new() -> Self {
        MultiChannelDenoiser {
            channels: 0,
            denoisers: Vec::new(),
            scratch_in: Vec::new(),
            scratch_out: Vec::new(),
        }
    }

    fn setup_denoisers(&mut self, channels: u16) {
        if channels != self.channels {
            self.denoisers = Vec::new();
            for _ in 0..channels {
                self.denoisers.push(T::init());
            }
            self.scratch_in = (0..channels).map(|_| vec![0.0; T::FRAME_SIZE]).collect();
            self.scratch_out = (0..channels).map(|_| vec![0.0; T::FRAME_SIZE]).collect();
            self.channels = channels;
        }
    }

    pub fn denoise_chunk(&mut self, mut chunk: AudioChunk) -> AudioChunk {
        let channels = chunk.format.channel_count;

        if channels == 0 || chunk.audio_data.is_empty() {
            return chunk;
        }
        self.setup_denoisers(channels);

        let ch = channels as usize;
        let frame_samples = ch * T::FRAME_SIZE;
        let full_len = (chunk.audio_data.len() / frame_samples) * frame_samples;
        for start in (0..full_len).step_by(frame_samples) {
            for c in 0..ch {
                for i in 0..T::FRAME_SIZE {
                    self.scratch_in[c][i] = chunk.audio_data[start + i * ch + c];
                }
            }
            for c in 0..ch {
                self.denoisers[c].process_frame(&mut self.scratch_out[c], &self.scratch_in[c]);
            }
            for i in 0..T::FRAME_SIZE {
                for c in 0..ch {
                    chunk.audio_data[start + i * ch + c] = self.scratch_out[c][i];
                }
            }
        }
        chunk
    }
}
