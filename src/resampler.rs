use std::collections::VecDeque;

use rubato::{Resampler, SincFixedIn};

use async_trait::async_trait;

use crate::{processor::AUDIO_CHUNK_SIZE, server::AudioReceiver};

pub struct ResampledAudioReceiver<R: AudioReceiver> {
    resampler: SincFixedIn<f32>,
    buffer: VecDeque<f32>,
    delegate: R,
    sample_rate: u32,
}

impl<R: AudioReceiver + Send> ResampledAudioReceiver<R> {
    pub fn new(delegate: R, sample_rate: u32) -> ResampledAudioReceiver<R> {
        let params = rubato::InterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: rubato::InterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        let resampler = SincFixedIn::<f32>::new(
            sample_rate as f64 / delegate.sample_rate() as f64,
            params,
            AUDIO_CHUNK_SIZE,
            delegate.channels() as usize,
        );
        ResampledAudioReceiver {
            resampler,
            buffer: VecDeque::new(),
            delegate,
            sample_rate,
        }
    }
}

fn separate_channels(samples: &[f32], channel_count: usize) -> Vec<Vec<f32>> {
    let mut channels = Vec::new();
    for _ in 0..channel_count {
        channels.push(Vec::new());
    }
    for (i, sample) in samples.iter().enumerate() {
        let channel = channels.get_mut(i % channel_count).unwrap();
        channel.push(*sample);
    }
    channels
}

fn interleave_channels(channels: &[Vec<f32>]) -> Vec<f32> {
    let mut samples = Vec::new();
    for i in 0..channels[0].len() {
        for channel in channels {
            samples.push(channel[i]);
        }
    }
    samples
}

#[async_trait]
impl<R: AudioReceiver + Send> AudioReceiver for ResampledAudioReceiver<R> {
    async fn next(&mut self) -> f32 {
        if self.delegate.sample_rate() == self.sample_rate {
            return self.delegate.next().await;
        } else {
            if self.buffer.is_empty() {
                let mut samples = Vec::new();
                let channel_count = self.delegate.channels();
                for _ in 0..(AUDIO_CHUNK_SIZE * channel_count as usize) {
                    samples.push(self.delegate.next().await);
                }
                let channels = separate_channels(&samples, self.delegate.channels() as usize);
                let resampled_channels = self.resampler.process(&channels).unwrap();
                let resampled_samples = interleave_channels(&resampled_channels);
                self.buffer = resampled_samples.into();
            }
            return self.buffer.pop_front().unwrap();
        }
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.delegate.channels()
    }
}
