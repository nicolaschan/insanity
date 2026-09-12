use std::collections::VecDeque;

use insanity_core::audio::{
    AudioFormat,
    sample::{Resampler, SampleSource, SyncSampleSource},
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
        }
    }
}

impl<R: SampleSource + Send> SampleSource for RubatoResampler<R> {
    async fn next(&mut self) -> Option<f32> {
        if self.source_format.sample_rate == self.target_rate {
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

pub struct StreamResampler {
    resampler: Option<SincFixedIn<f32>>,
    pending_in: VecDeque<f32>,
    pending_out: VecDeque<f32>,
    source_channels: usize,
    block_frames: usize,
}

impl StreamResampler {
    pub fn new(source: AudioFormat, target_rate: u32, block_frames: usize) -> Self {
        let bypass =
            source.sample_rate == target_rate || source.channel_count == 0 || block_frames == 0;
        let resampler = if bypass {
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
                target_rate as f64 / source.sample_rate as f64,
                params,
                block_frames,
                source.channel_count as usize,
            ))
        };
        StreamResampler {
            resampler,
            pending_in: VecDeque::new(),
            pending_out: VecDeque::new(),
            source_channels: source.channel_count as usize,
            block_frames,
        }
    }
}

impl Resampler for StreamResampler {
    fn push_sample(&mut self, sample: f32) {
        let Some(resampler) = self.resampler.as_mut() else {
            self.pending_out.push_back(sample);
            return;
        };
        self.pending_in.push_back(sample);
        let block = self.block_frames * self.source_channels;
        if self.pending_in.len() < block {
            return;
        }
        let input: Vec<f32> = self.pending_in.drain(..block).collect();
        let channels = split_channels(&input, self.source_channels);
        match resampler.process(&channels) {
            Ok(converted) => self.pending_out.extend(interleave_channels(&converted)),
            Err(_) => {
                log::error!("Resampler failed, passing chunk through unprocessed");
                self.pending_out.extend(input);
            }
        }
    }

    fn pop_sample(&mut self) -> Option<f32> {
        self.pending_out.pop_front()
    }

    fn buffered(&self) -> usize {
        self.pending_out.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{RubatoResampler, StreamResampler};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::{AudioChunk, ChunkSource, SampleChunker};
    use insanity_core::audio::sample::{Resampler, SampleSource};
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

    fn drain(resampler: &mut StreamResampler) -> Vec<f32> {
        let mut out = Vec::new();
        while let Some(sample) = resampler.pop_sample() {
            out.push(sample);
        }
        out
    }

    fn push_all(resampler: &mut StreamResampler, chunk: AudioChunk) {
        for sample in chunk.audio_data {
            resampler.push_sample(sample);
        }
    }

    #[test]
    fn equal_rates_pass_through_exactly() {
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 48000), 48000, 480);
        let data: Vec<f32> = (0..960).map(|v| v as f32).collect();
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 48000), data.clone()),
        );
        assert_eq!(drain(&mut resampler), data);
    }

    #[test]
    fn resample_yields_expected_sample_count() {
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 44100), 48000, 480);
        let mut total = 0usize;
        for seq in 0..20 {
            push_all(
                &mut resampler,
                AudioChunk::new(seq, AudioFormat::new(2, 44100), vec![0.4; 960]),
            );
            let out = drain(&mut resampler);
            assert!(out.iter().all(|sample| sample.is_finite()));
            if seq == 0 {
                continue;
            }
            assert!(out.len() == 1044 || out.len() == 1046);
            total += out.len();
        }
        let expected = 19.0 * 960.0 * 48000.0 / 44100.0;
        assert!(
            (total as f64 - expected).abs() < 19.0,
            "got {total}, expected {expected}"
        );
    }

    #[test]
    fn partial_block_waits_for_rest() {
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 44100), 48000, 480);
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 44100), vec![0.4; 100]),
        );
        assert_eq!(resampler.pop_sample(), None);
        push_all(
            &mut resampler,
            AudioChunk::new(1, AudioFormat::new(2, 44100), vec![0.4; 860]),
        );
        assert!(resampler.pop_sample().is_some());
    }

    #[test]
    fn degenerate_formats_do_not_panic() {
        let mut resampler = StreamResampler::new(AudioFormat::new(0, 48000), 48000, 480);
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(0, 48000), Vec::new()),
        );
        assert_eq!(resampler.pop_sample(), None);
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 44100), 48000, 0);
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 44100), vec![0.4; 960]),
        );
        assert_eq!(drain(&mut resampler), vec![0.4; 960]);
    }
}
