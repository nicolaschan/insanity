use std::collections::VecDeque;

use insanity_core::audio_source::{AudioSource, SyncAudioSource};
use log::trace;
use rubato::{Resampler, SincFixedIn};

pub struct ResampledAudioSource<R: AudioSource> {
    resampler: SincFixedIn<f32>,
    resampled_buffer: VecDeque<f32>,
    original_samples_buffer: VecDeque<f32>,
    delegate: R,
    sample_rate: u32,
    chunk_size: usize,
    bypass_hits: std::sync::atomic::AtomicUsize,
}

impl<R: AudioSource + Send + Sync> ResampledAudioSource<R> {
    pub fn new(delegate: R, sample_rate: u32, chunk_size: usize) -> ResampledAudioSource<R> {
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
            chunk_size,
            delegate.channels() as usize,
        );
        ResampledAudioSource {
            resampler,
            resampled_buffer: VecDeque::new(),
            original_samples_buffer: VecDeque::new(),
            delegate,
            sample_rate,
            chunk_size,
            bypass_hits: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Number of samples served via the zero-cost passthrough path
    pub fn bypass_hits(&self) -> usize {
        self.bypass_hits.load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn separate_channels(samples: &[f32], channel_count: usize) -> Vec<Vec<f32>> {
    let mut channels = Vec::new();
    for _ in 0..channel_count {
        channels.push(Vec::new());
    }
    if channel_count == 0 {
        return channels;
    }
    if !samples.len().is_multiple_of(channel_count) {
        log::error!(
            "non-frame-aligned input: {} samples for {} channels",
            samples.len(),
            channel_count
        );
    }
    for (i, sample) in samples.iter().enumerate() {
        channels[i % channel_count].push(*sample);
    }
    channels
}

fn interleave_channels(channels: &[Vec<f32>]) -> Vec<f32> {
    let Some(first) = channels.first() else {
        return Vec::new();
    };
    let mut samples = Vec::new();
    for i in 0..first.len() {
        for channel in channels {
            samples.push(channel[i]);
        }
    }
    samples
}

impl<R: AudioSource + Send> AudioSource for ResampledAudioSource<R> {
    async fn next(&mut self) -> Option<f32> {
        if self.delegate.sample_rate() == self.sample_rate {
            self.bypass_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return self.delegate.next().await;
        }
        if self.resampled_buffer.is_empty() {
            // First, try to fill the original_samples buffer with enough samples to resample
            let target_samples_count = self.chunk_size * self.delegate.channels() as usize;
            trace!(
                "Audio chunk size: {}, channels: {}, target samples count: {}",
                self.chunk_size,
                self.delegate.channels(),
                target_samples_count
            );
            if self.original_samples_buffer.len() < target_samples_count {
                for _ in 0..(target_samples_count - self.original_samples_buffer.len()) {
                    // ? operator returns none if there are not enough samples right now
                    let next_sample = self.delegate.next().await?;
                    self.original_samples_buffer.push_back(next_sample);
                }
            }

            // There are enough samples, so we can try to resample
            trace!(
                "Number of samples in original buffer: {}",
                self.original_samples_buffer.len()
            );
            let samples = self.original_samples_buffer.drain(..).collect::<Vec<f32>>();
            let channels = separate_channels(&samples, self.delegate.channels() as usize);
            trace!("Separated into {} channels", channels.len());
            let Ok(resampled_channels) = self.resampler.process(&channels) else {
                log::error!("Resampler failed, passing chunk through unprocessed");
                self.resampled_buffer = samples.into();
                return self.resampled_buffer.pop_front();
            };
            let resampled_samples = interleave_channels(&resampled_channels);
            self.resampled_buffer = resampled_samples.into();
        }
        self.resampled_buffer.pop_front()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.delegate.channels()
    }
}

impl<R: SyncAudioSource + Send> SyncAudioSource for ResampledAudioSource<R> {
    fn next_sync(&mut self) -> Option<f32> {
        if self.delegate.sample_rate() == self.sample_rate {
            self.bypass_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return self.delegate.next_sync();
        }
        if self.resampled_buffer.is_empty() {
            // First, try to fill the original_samples buffer with enough samples to resample
            let target_samples_count = self.chunk_size * self.delegate.channels() as usize;
            trace!(
                "Audio chunk size: {}, channels: {}, target samples count: {}",
                self.chunk_size,
                self.delegate.channels(),
                target_samples_count
            );
            if self.original_samples_buffer.len() < target_samples_count {
                for _ in 0..(target_samples_count - self.original_samples_buffer.len()) {
                    // ? operator returns none if there are not enough samples right now
                    let next_sample = self.delegate.next_sync()?;
                    self.original_samples_buffer.push_back(next_sample);
                }
            }

            // There are enough samples, so we can try to resample
            trace!(
                "Number of samples in original buffer: {}",
                self.original_samples_buffer.len()
            );
            let samples = self.original_samples_buffer.drain(..).collect::<Vec<f32>>();
            let channels = separate_channels(&samples, self.delegate.channels() as usize);
            trace!("Separated into {} channels", channels.len());
            let Ok(resampled_channels) = self.resampler.process(&channels) else {
                log::error!("Resampler failed (sync), passing chunk through unprocessed");
                self.resampled_buffer = samples.into();
                return self.resampled_buffer.pop_front();
            };
            let resampled_samples = interleave_channels(&resampled_channels);
            self.resampled_buffer = resampled_samples.into();
        }
        self.resampled_buffer.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::{interleave_channels, separate_channels};

    #[test]
    fn separate_empty_count_returns_empty() {
        assert!(separate_channels(&[0.1, 0.2], 0).is_empty());
        assert!(interleave_channels(&[]).is_empty());
    }

    #[test]
    fn separate_ragged_distributes_round_robin() {
        let out = separate_channels(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], vec![0.0, 3.0, 6.0]);
        assert_eq!(out[1], vec![1.0, 4.0]);
        assert_eq!(out[2], vec![2.0, 5.0]);
    }

    #[test]
    fn separate_aligned_roundtrips_through_interleave() {
        let samples: Vec<f32> = (0..12).map(|v| v as f32).collect();
        let channels = separate_channels(&samples, 3);
        assert_eq!(interleave_channels(&channels), samples);
    }
}
