use nnnoiseless::DenoiseState;
use serde::{Deserialize, Serialize};

pub const AUDIO_CHUNK_SIZE: usize = 480;
pub const AUDIO_CHANNELS: u16 = 2;

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

pub struct MultiChannelDenoiser<'a> {
    channels: u16,
    denoisers: Vec<DenoiseState<'a>>,
}

impl Default for MultiChannelDenoiser<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiChannelDenoiser<'_> {
    pub fn new() -> Self {
        let denoisers: Vec<DenoiseState> = Vec::new();
        MultiChannelDenoiser {
            channels: 0,
            denoisers,
        }
    }

    fn setup_denoisers(&mut self, channels: u16) {
        if channels != self.channels {
            self.denoisers = Vec::new();
            for _ in 0..channels {
                self.denoisers
                    .push(*DenoiseState::from_model(nnnoiseless::RnnModel::default()));
            }
            self.channels = channels;
        }
    }

    pub fn denoise_chunk(&mut self, chunk: &AudioChunk) -> AudioChunk {
        let magic = 32767.0;

        let mut denoised_output: Vec<f32> = Vec::new();

        let channels = chunk.audio_format.channel_count;
        // Degenerate input: nothing to denoise, preserve as-is.
        if channels == 0 || chunk.audio_data.is_empty() {
            return chunk.clone();
        }
        self.setup_denoisers(channels);

        let frame_samples = (channels as usize) * DenoiseState::FRAME_SIZE;
        let full_len = (chunk.audio_data.len() / frame_samples) * frame_samples;
        for audio_chunk in chunk.audio_data[..full_len].chunks_exact(frame_samples) {
            // Audio data for each channel is interleaved
            // Separate it into a buffer for each channel in the raw_audio Vec
            let mut raw_audio: Vec<[f32; DenoiseState::FRAME_SIZE]> = Vec::new();
            for _ in 0..channels {
                raw_audio.push([0.0; DenoiseState::FRAME_SIZE]);
            }
            let mut denoised_audio: Vec<[f32; DenoiseState::FRAME_SIZE]> = Vec::new();
            for (i, val) in audio_chunk.iter().enumerate() {
                raw_audio[i % (channels as usize)][i / (channels as usize)] = *val * magic;
            }

            // Denoise each channel independently
            for i in 0..channels {
                let mut denoiser = self.denoisers.swap_remove(i as usize);
                let mut denoised_audio_buffer = [0.0; DenoiseState::FRAME_SIZE];
                denoiser.process_frame(&mut denoised_audio_buffer, &raw_audio[i as usize]);
                self.denoisers.insert(i as usize, denoiser);
                denoised_audio.insert(i as usize, denoised_audio_buffer);
            }

            // Re-interleave the audio data (transpose channel frames).
            denoised_output.extend(
                (0..DenoiseState::FRAME_SIZE)
                    .flat_map(|i| denoised_audio.iter().map(move |ch| ch[i] / magic)),
            );
        }
        // Tail that doesn't fill a full denoiser frame can't be processed;
        // pass it through unchanged so no samples are lost.
        denoised_output.extend_from_slice(&chunk.audio_data[full_len..]);

        AudioChunk::new(
            chunk.sequence_number,
            chunk.audio_format.clone(),
            denoised_output,
        )
    }
}
