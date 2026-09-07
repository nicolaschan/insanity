use std::collections::VecDeque;

use insanity_core::audio::{
    AudioFormat,
    sample::{SampleSource, SyncSampleSource},
    sample_ops::{interleave_channels, split_channels},
};
use log::trace;
use rubato::{Resampler, SincFixedIn};

pub struct ResampledAudioSource<R: SampleSource> {
    resampler: SincFixedIn<f32>,
    resampled_buffer: VecDeque<f32>,
    original_samples_buffer: VecDeque<f32>,
    delegate: R,
    sample_rate: u32,
    chunk_size: usize,
    bypass_hits: std::sync::atomic::AtomicUsize,
}

impl<R: SampleSource + Send + Sync> ResampledAudioSource<R> {
    pub fn new(delegate: R, sample_rate: u32, chunk_size: usize) -> ResampledAudioSource<R> {
        let params = rubato::InterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: rubato::InterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        let resampler = SincFixedIn::<f32>::new(
            sample_rate as f64 / delegate.format().sample_rate as f64,
            params,
            chunk_size,
            delegate.format().channel_count as usize,
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

impl<R: SampleSource + Send> SampleSource for ResampledAudioSource<R> {
    async fn next(&mut self) -> Option<f32> {
        if self.delegate.format().sample_rate == self.sample_rate {
            self.bypass_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return self.delegate.next().await;
        }
        if self.resampled_buffer.is_empty() {
            // First, try to fill the original_samples buffer with enough samples to resample
            let target_samples_count =
                self.chunk_size * self.delegate.format().channel_count as usize;
            trace!(
                "Audio chunk size: {}, channels: {}, target samples count: {}",
                self.chunk_size,
                self.delegate.format().channel_count,
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
            let channels = split_channels(&samples, self.delegate.format().channel_count as usize);
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

    fn format(&self) -> AudioFormat {
        AudioFormat::new(self.delegate.format().channel_count, self.sample_rate)
    }
}

impl<R: SyncSampleSource + Send> SyncSampleSource for ResampledAudioSource<R> {
    fn next_sync(&mut self) -> Option<f32> {
        if self.delegate.format().sample_rate == self.sample_rate {
            self.bypass_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return self.delegate.next_sync();
        }
        if self.resampled_buffer.is_empty() {
            // First, try to fill the original_samples buffer with enough samples to resample
            let target_samples_count =
                self.chunk_size * self.delegate.format().channel_count as usize;
            trace!(
                "Audio chunk size: {}, channels: {}, target samples count: {}",
                self.chunk_size,
                self.delegate.format().channel_count,
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
            let channels = split_channels(&samples, self.delegate.format().channel_count as usize);
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
