use crate::audio::AudioChunk;

pub trait Denoiser {
    const FRAME_SIZE: usize;
    fn init() -> Self;
    fn process_frame(&mut self, output: &mut [f32], input: &[f32]);
}

pub struct MultiChannelDenoiser<T: Denoiser> {
    channels: u16,
    denoisers: Vec<T>,
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
        }
    }

    fn setup_denoisers(&mut self, channels: u16) {
        if channels != self.channels {
            self.denoisers = Vec::new();
            for _ in 0..channels {
                self.denoisers.push(T::init());
            }
            self.channels = channels;
        }
    }

    pub fn denoise_chunk(&mut self, chunk: &AudioChunk) -> AudioChunk {
        let mut denoised_audio: Vec<f32> = Vec::new();

        let channels = chunk.audio_format.channel_count;
        self.setup_denoisers(channels);

        for audio_chunk in chunk
            .audio_data
            .chunks_exact((channels as usize) * T::FRAME_SIZE)
        {
            let raw_audio = split_channels(audio_chunk, channels as usize);

            // Denoise each channel independently
            let mut denoised_channels = Vec::new();
            for i in 0..channels {
                let mut denoiser = self.denoisers.swap_remove(i as usize);
                let mut denoised_audio_buffer = vec![0.0; T::FRAME_SIZE];
                denoiser.process_frame(&mut denoised_audio_buffer, &raw_audio[i as usize]);
                self.denoisers.insert(i as usize, denoiser);
                denoised_channels.push(denoised_audio_buffer);
            }

            for sample in interleave_channels(&denoised_channels) {
                denoised_audio.push(sample);
            }
        }

        AudioChunk::new(
            chunk.sequence_number,
            chunk.audio_format.clone(),
            denoised_audio,
        )
    }
}

fn split_channels(audio_chunk: &[f32], num_channels: usize) -> Vec<Vec<f32>> {
    let mut channels: Vec<Vec<f32>> = vec![Vec::new(); num_channels];
    for (i, &val) in audio_chunk.iter().enumerate() {
        let channel_index = i % num_channels;
        let mut channel = channels.swap_remove(channel_index);
        channel.push(val);
        channels.insert(channel_index, channel);
    }
    channels
}

fn interleave_channels(channels: &[Vec<f32>]) -> Vec<f32> {
    let mut samples = Vec::new();
    let frame_size = channels[0].len();
    for i in 0..frame_size {
        for c in channels.iter() {
            samples.push(c[i]);
        }
    }
    samples
}
