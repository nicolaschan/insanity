use std::collections::VecDeque;

use insanity_core::audio::{
    AudioFormat,
    chunk::{AudioChunk, ChunkSource},
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

/// Resamples a chunk stream to `target_rate`, rebuilding on format change.
pub struct ResampledChunkSource<S> {
    source: S,
    target_rate: u32,
    block_frames: usize,
    resampler: Option<(AudioFormat, RubatoResampler)>,
    pending: VecDeque<f32>,
    next_sequence: u128,
}

impl<S: ChunkSource + Send> ResampledChunkSource<S> {
    pub fn new(source: S, target_rate: u32, block_frames: usize) -> Self {
        ResampledChunkSource {
            source,
            target_rate,
            block_frames,
            resampler: None,
            pending: VecDeque::new(),
            next_sequence: 0,
        }
    }

    fn next_sequence(&mut self) -> u128 {
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        sequence_number
    }

    fn take_ready(&mut self) -> Option<AudioChunk> {
        let (format, resampler) = self.resampler.as_mut()?;
        let len = resampler.input_block_frames() * format.channel_count as usize;
        if len == 0 || self.pending.len() < len {
            return None;
        }
        let block: Vec<f32> = self.pending.drain(..len).collect();
        let audio_data = resampler.resample(&block);
        let format = AudioFormat::new(format.channel_count, self.target_rate);
        let sequence_number = self.next_sequence();
        Some(AudioChunk::new(sequence_number, format, audio_data))
    }

    fn adopt(&mut self, format: &AudioFormat) {
        if self.resampler.as_ref().map(|(f, _)| f) == Some(format) {
            return;
        }
        self.pending.clear();
        self.resampler = Some((
            format.clone(),
            RubatoResampler::new(
                format.sample_rate,
                self.target_rate,
                format.channel_count,
                self.block_frames,
            ),
        ));
    }
}

impl<S: ChunkSource + Send> ChunkSource for ResampledChunkSource<S> {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        loop {
            if let Some(chunk) = self.take_ready() {
                return Some(chunk);
            }
            let mut chunk = self.source.next_chunk().await?;
            if chunk.format.sample_rate == self.target_rate {
                self.resampler = None;
                self.pending.clear();
                chunk.sequence_number = self.next_sequence();
                return Some(chunk);
            }
            self.adopt(&chunk.format);
            self.pending.extend(chunk.audio_data);
        }
    }
}

pub struct RubatoResampler {
    inner: Option<SincFixedIn<f32>>,
    channels: u16,
    block_frames: usize,
}

impl RubatoResampler {
    pub fn new(source_rate: u32, target_rate: u32, channels: u16, block_frames: usize) -> Self {
        if source_rate == target_rate {
            return RubatoResampler {
                inner: None,
                channels,
                block_frames,
            };
        }
        let params = rubato::InterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: rubato::InterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        RubatoResampler {
            inner: Some(SincFixedIn::<f32>::new(
                target_rate as f64 / source_rate as f64,
                params,
                block_frames,
                channels as usize,
            )),
            channels,
            block_frames,
        }
    }
}

impl Resampler for RubatoResampler {
    fn input_block_frames(&self) -> usize {
        self.block_frames
    }

    fn resample(&mut self, input: &[f32]) -> Vec<f32> {
        let Some(inner) = self.inner.as_mut() else {
            return input.to_vec();
        };
        let separated = split_channels(input, self.channels as usize);
        match inner.process(&separated) {
            Ok(outputs) => interleave_channels(&outputs),
            Err(e) => {
                log::error!("Resampler failed: {e:?}, passing block through unprocessed");
                input.to_vec()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RubatoResampler;
    use insanity_core::audio::resample::Resampler;

    #[test]
    fn passthrough_equal_rates() {
        let mut resampler = RubatoResampler::new(48000, 48000, 2, 480);
        assert_eq!(resampler.input_block_frames(), 480);
        let input: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();
        assert_eq!(resampler.resample(&input), input);
    }

    #[test]
    fn resample_tracks_rate_ratio_after_priming() {
        const RESAMPLE_ERROR_TOL_NUM_SAMPLES: f64 = 2.1;
        for (source, target) in [(44100, 48000), (48000, 44100)] {
            let mut resampler = RubatoResampler::new(source, target, 2, 480);
            let input = vec![0.5f32; 960];
            for _ in 0..4 {
                let priming = resampler.resample(&input);
                assert!(priming.iter().all(|s| s.is_finite()));
            }
            let mut total_in = 0usize;
            let mut total_out = 0usize;
            for _ in 0..20 {
                let out = resampler.resample(&input);
                assert!(out.iter().all(|s| s.is_finite()));
                total_in += input.len();
                total_out += out.len();
            }
            let expected = total_in as f64 * target as f64 / source as f64;
            let actual = total_out as f64;
            assert!(
                (actual - expected).abs() < RESAMPLE_ERROR_TOL_NUM_SAMPLES,
                "rates {source}->{target}: got {actual}, expected {expected}"
            );
        }
    }
}

#[cfg(test)]
mod chunk_source_tests {
    use super::ResampledChunkSource;
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
    use std::collections::VecDeque;
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

    struct Scripted(VecDeque<AudioChunk>);

    impl ChunkSource for Scripted {
        async fn next_chunk(&mut self) -> Option<AudioChunk> {
            self.0.pop_front()
        }
    }

    #[test]
    fn matching_rate_passes_through_with_own_sequence() {
        let format = AudioFormat::new(2, 48000);
        let source = Scripted(VecDeque::from(vec![
            AudioChunk::new(9, format.clone(), vec![0.5; 4]),
            AudioChunk::new(9, format.clone(), vec![0.25; 6]),
        ]));
        let mut resampled = ResampledChunkSource::new(source, 48000, 480);
        let first = block_on(resampled.next_chunk()).expect("chunk");
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.audio_data, vec![0.5; 4]);
        let second = block_on(resampled.next_chunk()).expect("chunk");
        assert_eq!(second.sequence_number, 1);
        assert_eq!(second.audio_data, vec![0.25; 6]);
        assert!(block_on(resampled.next_chunk()).is_none());
    }

    #[test]
    fn mismatched_rate_yields_target_format_at_rate_ratio() {
        let format = AudioFormat::new(2, 44100);
        let chunks = (0..24)
            .map(|i| AudioChunk::new(i, format.clone(), vec![0.5; 960]))
            .collect();
        let mut resampled = ResampledChunkSource::new(Scripted(chunks), 48000, 480);
        let mut total_out = 0usize;
        let mut count = 0usize;
        while let Some(chunk) = block_on(resampled.next_chunk()) {
            assert_eq!(chunk.format, AudioFormat::new(2, 48000));
            assert!(chunk.audio_data.iter().all(|s| s.is_finite()));
            if count >= 4 {
                total_out += chunk.audio_data.len();
            }
            count += 1;
        }
        assert_eq!(count, 24);
        let expected = 20.0 * 960.0 * 48000.0 / 44100.0;
        assert!((total_out as f64 - expected).abs() < 2.1 * 20.0);
    }

    #[test]
    fn format_change_rebuilds_and_drops_partial_block() {
        let first = AudioFormat::new(1, 44100);
        let second = AudioFormat::new(2, 44100);
        let source = Scripted(VecDeque::from(vec![
            AudioChunk::new(0, first, vec![0.5; 100]),
            AudioChunk::new(1, second.clone(), vec![0.5; 960]),
        ]));
        let mut resampled = ResampledChunkSource::new(source, 48000, 480);
        let chunk = block_on(resampled.next_chunk()).expect("chunk");
        assert_eq!(chunk.format, AudioFormat::new(2, 48000));
        assert!(block_on(resampled.next_chunk()).is_none());
    }
}
