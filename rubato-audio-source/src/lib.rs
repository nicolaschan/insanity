use std::collections::VecDeque;

use futures_util::{StreamExt, future, stream};
use insanity_core::audio::{
    AudioFormat,
    sample::{Resampler, SampleSource},
    sample_ops::{interleave_channels, split_channels},
};
use log::error;
use rubato::{Resampler as RubatoResamplerTrait, SincFixedIn};

fn sinc_params() -> rubato::InterpolationParameters {
    rubato::InterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: rubato::InterpolationType::Linear,
        oversampling_factor: 256,
        window: rubato::WindowFunction::BlackmanHarris2,
    }
}

pub fn resample(
    source: impl SampleSource,
    target_rate: u32,
    chunk_size: usize,
) -> impl SampleSource {
    let source_format = source.format().clone();
    let out_format = AudioFormat::new(source_format.channel_count, target_rate);
    let channels = source_format.channel_count as usize;
    let Some(mut resampler) =
        build_sinc(source_format.sample_rate, channels, target_rate, chunk_size)
    else {
        return source.with_format(out_format, |samples| samples.left_stream());
    };
    let block = chunk_size * channels;
    source.with_format(out_format, move |samples| {
        samples
            .chunks(block)
            .take_while(move |samples| future::ready(samples.len() == block))
            .map(move |samples| stream::iter(resample_block(&mut resampler, samples, channels)))
            .flatten()
            .right_stream()
    })
}

fn resample_block(
    resampler: &mut SincFixedIn<f32>,
    samples: Vec<f32>,
    channels: usize,
) -> Vec<f32> {
    let split = split_channels(&samples, channels);
    match resampler.process(&split) {
        Ok(converted) => interleave_channels(&converted),
        Err(_) => {
            error!("Resampler failed, passing chunk through unprocessed");
            samples
        }
    }
}

pub struct StreamResampler {
    resampler: Option<SincFixedIn<f32>>,
    pending_in: VecDeque<f32>,
    pending_out: VecDeque<f32>,
    source_channels: usize,
    source_rate: u32,
    target_rate: u32,
    block_frames: usize,
}

fn build_sinc(
    source_rate: u32,
    source_channels: usize,
    target_rate: u32,
    block_frames: usize,
) -> Option<SincFixedIn<f32>> {
    if source_rate == 0
        || target_rate == 0
        || source_rate == target_rate
        || source_channels == 0
        || block_frames == 0
    {
        return None;
    }
    let params = sinc_params();
    Some(SincFixedIn::<f32>::new(
        target_rate as f64 / source_rate as f64,
        params,
        block_frames,
        source_channels,
    ))
}

impl StreamResampler {
    pub fn new(source: AudioFormat, target_rate: u32, block_frames: usize) -> Self {
        StreamResampler {
            resampler: build_sinc(
                source.sample_rate,
                source.channel_count as usize,
                target_rate,
                block_frames,
            ),
            pending_in: VecDeque::new(),
            pending_out: VecDeque::new(),
            source_channels: source.channel_count as usize,
            source_rate: source.sample_rate,
            target_rate,
            block_frames,
        }
    }

    fn resample_into(&mut self, samples: Vec<f32>, channels: usize) -> VecDeque<f32> {
        match self.resampler.as_mut() {
            Some(resampler) => resample_block(resampler, samples, channels).into(),
            None => samples.into(),
        }
    }

    fn block_samples(&self) -> usize {
        self.block_frames * self.source_channels
    }

    fn process_block(&mut self, input: Vec<f32>) {
        let channels = self.source_channels;
        let out = self.resample_into(input, channels);
        self.pending_out.extend(out);
    }

    fn drain_full_blocks(&mut self) {
        let block = self.block_samples();
        if block == 0 {
            return;
        }
        while self.pending_in.len() >= block {
            let input: Vec<f32> = self.pending_in.drain(..block).collect();
            self.process_block(input);
        }
    }
}

impl Resampler for StreamResampler {
    fn push_sample(&mut self, sample: f32) {
        if self.resampler.is_none() {
            self.pending_out.push_back(sample);
            return;
        }
        self.pending_in.push_back(sample);
        self.drain_full_blocks();
    }

    fn pop_sample(&mut self) -> Option<f32> {
        self.pending_out.pop_front()
    }

    fn buffered(&self) -> usize {
        if self.resampler.is_none() {
            return self.pending_out.len();
        }
        let estimate =
            self.pending_in.len() as u64 * self.target_rate as u64 / self.source_rate as u64;
        self.pending_out.len() + estimate as usize
    }

    fn reset(&mut self) {
        self.pending_in.clear();
        self.pending_out.clear();
    }

    fn reconfigure(
        &mut self,
        source_rate: u32,
        source_channels: usize,
        target_rate: u32,
        block_frames: usize,
    ) {
        self.resampler = build_sinc(source_rate, source_channels, target_rate, block_frames);
        self.reset();
        self.source_channels = source_channels;
        self.source_rate = source_rate;
        self.target_rate = target_rate;
        self.block_frames = block_frames;
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamResampler, resample};
    use futures_util::{StreamExt, stream};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::AudioChunk;
    use insanity_core::audio::sample::{Resampler, SampleSource, Sampled};

    fn sine(sample_rate: u32, freq: f32) -> impl SampleSource {
        let mut index = 0u64;
        let samples = stream::repeat_with(move || {
            let t = index as f32 / sample_rate as f32;
            index += 1;
            (2.0 * std::f32::consts::PI * freq * t).sin()
        });
        Sampled::new(AudioFormat::new(2, sample_rate), samples)
    }

    #[tokio::test]
    async fn resampled_device_path_yields_exact_opus_frames() {
        let target = AudioFormat::new(2, 48000);
        let mut chunker = resample(sine(44100, 440.0), 48000, 480)
            .into_chunks(480)
            .boxed();
        let mut total = 0usize;
        for _ in 0..20 {
            let chunk = chunker.next().await.expect("resampled stream is infinite");
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
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 44100), vec![0.4; 960]),
        );
        let _ = drain(&mut resampler);
        let mut total = 0usize;
        for seq in 1..20 {
            push_all(
                &mut resampler,
                AudioChunk::new(seq, AudioFormat::new(2, 44100), vec![0.4; 960]),
            );
            let out = drain(&mut resampler);
            assert!(out.iter().all(|sample| sample.is_finite()));
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

    #[test]
    fn zero_rates_bypass_without_panic() {
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 0), 48000, 480);
        let data = vec![0.4; 100];
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 0), data.clone()),
        );
        assert_eq!(drain(&mut resampler), data);
    }

    #[test]
    fn buffered_counts_partial_input() {
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 44100), 48000, 480);
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 44100), vec![0.4; 100]),
        );
        assert_eq!(resampler.buffered(), 100 * 48000 / 44100);
        assert_eq!(resampler.pop_sample(), None);
    }

    #[test]
    fn reset_discards_pending() {
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 44100), 48000, 480);
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 44100), vec![0.4; 100]),
        );
        resampler.reset();
        assert_eq!(resampler.buffered(), 0);
        assert_eq!(resampler.pop_sample(), None);
    }

    #[test]
    fn reconfigure_retunes_ratio() {
        let mut resampler = StreamResampler::new(AudioFormat::new(2, 44100), 48000, 480);
        resampler.reconfigure(48000, 2, 48000, 480);
        let data: Vec<f32> = (0..960).map(|v| v as f32).collect();
        push_all(
            &mut resampler,
            AudioChunk::new(0, AudioFormat::new(2, 48000), data.clone()),
        );
        assert_eq!(drain(&mut resampler), data);
    }
}
