use std::collections::VecDeque;

use insanity_core::audio::{
    AudioFormat,
    chunk::AudioChunk,
    resample::Resampler,
    sample::{SampleSource, SyncSampleSource},
    sample_ops::{interleave_channels, split_channels},
};
use log::trace;
use rubato::{Resampler as RubatoResamplerTrait, SincFixedIn};

pub struct ResampledAudioSource<R: SampleSource> {
    resampler: SincFixedIn<f32>,
    resampled_buffer: VecDeque<f32>,
    original_samples_buffer: VecDeque<f32>,
    delegate: R,
    source_format: AudioFormat,
    target_rate: u32,
    chunk_size: usize,
    bypass_hits: std::sync::atomic::AtomicUsize,
}

impl<R: SampleSource + Send + Sync> ResampledAudioSource<R> {
    pub fn new(
        delegate: R,
        source_format: AudioFormat,
        target_rate: u32,
        chunk_size: usize,
    ) -> ResampledAudioSource<R> {
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
        ResampledAudioSource {
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

impl<R: SampleSource + Send> SampleSource for ResampledAudioSource<R> {
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

impl<R: SyncSampleSource + Send> SyncSampleSource for ResampledAudioSource<R> {
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

pub struct RubatoResampler {
    inner: Option<SincFixedIn<f32>>,
    current: Option<AudioFormat>,
    target_rate: u32,
    block_frames: usize,
    staging: VecDeque<f32>,
}

impl RubatoResampler {
    pub fn new(target_rate: u32, block_frames: usize) -> Self {
        assert!(block_frames > 0);
        RubatoResampler {
            inner: None,
            current: None,
            target_rate,
            block_frames,
            staging: VecDeque::new(),
        }
    }

    fn ensure(&mut self, format: &AudioFormat) {
        if self.current.as_ref() == Some(format) {
            return;
        }
        self.staging.clear();
        self.current = Some(format.clone());
        self.inner = if format.sample_rate == self.target_rate {
            None
        } else {
            let params = rubato::InterpolationParameters {
                sinc_len: 256,
                f_cutoff: 0.95,
                interpolation: rubato::InterpolationType::Linear,
                oversampling_factor: 256,
                window: rubato::WindowFunction::BlackmanHarris2,
            };
            Some(SincFixedIn::<f32>::new(
                self.target_rate as f64 / format.sample_rate as f64,
                params,
                self.block_frames,
                format.channel_count as usize,
            ))
        };
    }
}

impl Resampler for RubatoResampler {
    fn resample(&mut self, chunk: &AudioChunk) -> AudioChunk {
        self.ensure(&chunk.format);
        let channels = chunk.format.channel_count as usize;
        if channels == 0 {
            return AudioChunk::new(
                chunk.sequence_number,
                AudioFormat::new(0, self.target_rate),
                Vec::new(),
            );
        }
        let Some(inner) = self.inner.as_mut() else {
            return chunk.clone();
        };
        self.staging.extend(chunk.audio_data.iter());
        let mut audio_data = Vec::new();
        while self.staging.len() >= self.block_frames * channels {
            let block: Vec<f32> = self.staging.drain(..self.block_frames * channels).collect();
            let separated = split_channels(&block, channels);
            match inner.process(&separated) {
                Ok(outputs) => audio_data.extend(interleave_channels(&outputs)),
                Err(e) => {
                    log::error!("Resampler failed: {e:?}, passing block through unprocessed");
                    audio_data.extend(block);
                }
            }
        }
        AudioChunk::new(
            chunk.sequence_number,
            AudioFormat::new(chunk.format.channel_count, self.target_rate),
            audio_data,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::RubatoResampler;
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::AudioChunk;
    use insanity_core::audio::resample::Resampler;

    fn stereo_chunk(sequence_number: u128, sample_rate: u32, value: f32) -> AudioChunk {
        AudioChunk::new(
            sequence_number,
            AudioFormat::new(2, sample_rate),
            vec![value; 960],
        )
    }

    #[test]
    fn passthrough_equal_rates() {
        let mut resampler = RubatoResampler::new(48000, 480);
        let chunk = AudioChunk::new(3, AudioFormat::new(2, 48000), vec![0.5; 960]);
        assert_eq!(resampler.resample(&chunk), chunk);
    }

    #[test]
    fn resample_tracks_rate_ratio_after_priming() {
        const RESAMPLE_ERROR_TOL_NUM_SAMPLES: f64 = 2.0;
        for (source, target) in [(44100, 48000), (48000, 44100)] {
            let mut resampler = RubatoResampler::new(target, 480);
            for sequence_number in 0..4u128 {
                let priming = resampler.resample(&stereo_chunk(sequence_number, source, 0.5));
                assert!(priming.audio_data.iter().all(|s| s.is_finite()));
            }
            let mut total_in = 0usize;
            let mut total_out = 0usize;
            for sequence_number in 4..24u128 {
                let out = resampler.resample(&stereo_chunk(sequence_number, source, 0.5));
                assert!(out.audio_data.iter().all(|s| s.is_finite()));
                assert_eq!(out.sequence_number, sequence_number);
                assert_eq!(out.format, AudioFormat::new(2, target));
                total_in += 960;
                total_out += out.audio_data.len();
            }
            let expected = total_in as f64 * target as f64 / source as f64;
            assert!(
                (total_out as f64 - expected).abs() < RESAMPLE_ERROR_TOL_NUM_SAMPLES,
                "rates {source}->{target}: got {total_out}, expected {expected}"
            );
        }
    }

    #[test]
    fn format_change_rebuilds_and_drops_partial() {
        let mut resampler = RubatoResampler::new(48000, 480);
        let partial = AudioChunk::new(0, AudioFormat::new(1, 44100), vec![1.0; 100]);
        let _ = resampler.resample(&partial);
        let mut total = 0usize;
        for sequence_number in 1..25u128 {
            let out = resampler.resample(&stereo_chunk(sequence_number, 44100, 0.0));
            assert_eq!(out.format, AudioFormat::new(2, 48000));
            if sequence_number >= 5 {
                assert!(out.audio_data.iter().all(|&s| s == 0.0));
                total += out.audio_data.len();
            }
        }
        let expected = 20.0 * 960.0 * 48000.0 / 44100.0;
        assert!((total as f64 - expected).abs() < 2.0);
    }

    #[test]
    fn zero_channel_chunk_yields_empty() {
        let mut resampler = RubatoResampler::new(48000, 480);
        let chunk = AudioChunk::new(0, AudioFormat::new(0, 44100), vec![0.5; 10]);
        let out = resampler.resample(&chunk);
        assert!(out.audio_data.is_empty());
    }
}
