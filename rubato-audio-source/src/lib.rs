use std::collections::VecDeque;

use insanity_core::audio::{
    AudioFormat,
    sample::{SampleSource, SyncSampleSource},
    sample_ops::{interleave_channels, split_channels},
};
use log::trace;
use rubato::{Resampler as RubatoResamplerTrait, SincFixedIn};

pub struct RubatoResampler<R: SampleSource> {
    resampler: SincFixedIn<f32>,
    resampled_buffer: VecDeque<f32>,
    original_samples_buffer: VecDeque<f32>,
    delegate: R,
    source_format: AudioFormat,
    target_rate: u32,
    chunk_size: usize,
    bypass_hits: std::sync::atomic::AtomicUsize,
}

impl<R: SampleSource + Send> RubatoResampler<R> {
    pub fn new(
        delegate: R,
        source_format: AudioFormat,
        target_rate: u32,
        chunk_size: usize,
    ) -> RubatoResampler<R> {
        let params = rubato::InterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: rubato::InterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        let resampler = SincFixedIn::<f32>::new(
            target_rate as f64 / source_format.sample_rate as f64,
            params,
            chunk_size,
            source_format.channel_count as usize,
        );
        RubatoResampler {
            resampler,
            resampled_buffer: VecDeque::new(),
            original_samples_buffer: VecDeque::new(),
            delegate,
            source_format,
            target_rate,
            chunk_size,
            bypass_hits: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Number of samples served via the zero-cost passthrough path
    pub fn bypass_hits(&self) -> usize {
        self.bypass_hits.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl<R: SampleSource + Send> SampleSource for RubatoResampler<R> {
    async fn next(&mut self) -> Option<f32> {
        if self.source_format.sample_rate == self.target_rate {
            self.bypass_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return self.delegate.next().await;
        }
        if self.resampled_buffer.is_empty() {
            // First, try to fill the original_samples buffer with enough samples to resample
            let target_samples_count = self.chunk_size * self.source_format.channel_count as usize;
            trace!(
                "Audio chunk size: {}, channels: {}, target samples count: {}",
                self.chunk_size, self.source_format.channel_count, target_samples_count
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
            let channels = split_channels(&samples, self.source_format.channel_count as usize);
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
}

impl<R: SyncSampleSource + Send> SyncSampleSource for RubatoResampler<R> {
    fn next_sync(&mut self) -> Option<f32> {
        if self.source_format.sample_rate == self.target_rate {
            self.bypass_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return self.delegate.next_sync();
        }
        if self.resampled_buffer.is_empty() {
            // First, try to fill the original_samples buffer with enough samples to resample
            let target_samples_count = self.chunk_size * self.source_format.channel_count as usize;
            trace!(
                "Audio chunk size: {}, channels: {}, target samples count: {}",
                self.chunk_size, self.source_format.channel_count, target_samples_count
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
            let channels = split_channels(&samples, self.source_format.channel_count as usize);
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
    use super::RubatoResampler;
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::{ChunkSource, SampleChunker};
    use insanity_core::audio::sample::SampleSource;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    struct Sine {
        sample_rate: u32,
        freq: f32,
        index: u64,
    }

    impl SampleSource for Sine {
        async fn next(&mut self) -> Option<f32> {
            let t = self.index as f32 / self.sample_rate as f32;
            self.index += 1;
            Some((2.0 * std::f32::consts::PI * self.freq * t).sin())
        }
    }

    #[test]
    fn resampled_device_path_yields_exact_opus_frames() {
        let source_format = AudioFormat::new(2, 44100);
        let resampled = RubatoResampler::new(
            Sine {
                sample_rate: 44100,
                freq: 440.0,
                index: 0,
            },
            source_format,
            48000,
            480,
        );
        let target = AudioFormat::new(2, 48000);
        let mut chunker = SampleChunker::new(resampled, 480, target.clone());
        let mut total = 0usize;
        for _ in 0..20 {
            let chunk = block_on(chunker.next_chunk()).expect("resampled stream is infinite");
            assert_eq!(chunk.format, target);
            assert_eq!(chunk.audio_data.len(), 960);
            total += chunk.audio_data.len();
        }
        let expected = 20.0 * 960.0;
        assert!(
            (total as f64 - expected).abs() < 2.0,
            "got {total}, expected {expected}"
        );
    }
}
