use std::{collections::VecDeque, sync::Mutex};

use rubato::{Resampler, SincFixedIn};

use async_trait::async_trait;

use crate::{processor::AUDIO_CHUNK_SIZE, server::AudioReceiver};

pub struct ResampledAudioReceiver<R: AudioReceiver> {
    resampler: Mutex<SincFixedIn<f32>>,
    resampled_buffer: VecDeque<f32>,
    original_samples_buffer: VecDeque<f32>,
    delegate: R,
    sample_rate: u32,
}

impl<R: AudioReceiver + Send + Sync> ResampledAudioReceiver<R> {
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
            resampler: Mutex::new(resampler),
            resampled_buffer: VecDeque::new(),
            original_samples_buffer: VecDeque::new(),
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
    async fn next(&mut self) -> Option<f32> {
        if self.delegate.sample_rate() == self.sample_rate {
            return self.delegate.next().await;
        }
        if self.resampled_buffer.is_empty() {
            // First, try to fill the original_samples buffer with enough samples to resample
            let target_samples_count = AUDIO_CHUNK_SIZE * self.delegate.channels() as usize;
            if self.original_samples_buffer.len() < target_samples_count {
                for _ in 0..(self.original_samples_buffer.len() - target_samples_count) {
                    let next_sample = self.delegate.next().await;
                    if next_sample.is_none() {
                        // Not enough samples available right now, so return None
                        return None;
                    }
                    self.original_samples_buffer.push_back(next_sample.unwrap());
                }
            }

            // There are enough samples, so we can try to resample
            let samples = self.original_samples_buffer.drain(..).collect::<Vec<f32>>();
            let channels = separate_channels(&samples, self.delegate.channels() as usize);
            let mut resampler_guard = self.resampler.lock().unwrap();
            let resampled_channels = resampler_guard.process(&channels).unwrap();
            let resampled_samples = interleave_channels(&resampled_channels);
            self.resampled_buffer = resampled_samples.into();
        }
        return self.resampled_buffer.pop_front();
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.delegate.channels()
    }
}
