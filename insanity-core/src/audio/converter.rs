use std::collections::VecDeque;

use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;
use crate::audio::sample::{Resampler, ResamplerSpec};
use crate::audio::sample_ops::convert_to_mixer_channels;

pub struct FormatConverter<R: Resampler> {
    from: AudioFormat,
    to: AudioFormat,
    block_samples: usize,
    capacity_samples: usize,
    resampler: R,
    pending: VecDeque<f32>,
}

impl<R: Resampler + From<ResamplerSpec>> FormatConverter<R> {
    pub fn new(
        from: AudioFormat,
        to: AudioFormat,
        block_samples: usize,
        capacity_samples: usize,
    ) -> Self {
        assert!(to.channel_count > 0);
        assert!(from.channel_count > 0);
        let channels = usize::from(to.channel_count);
        let resampler = R::from(ResamplerSpec {
            source_rate: from.sample_rate,
            source_channels: channels,
            target_rate: to.sample_rate,
            block_frames: block_samples / channels,
        });
        Self {
            from,
            to,
            block_samples,
            capacity_samples,
            resampler,
            pending: VecDeque::new(),
        }
    }

    pub fn pending_samples(&self) -> usize {
        self.pending.len()
    }

    pub fn feed(&mut self, samples: Vec<f32>) -> usize {
        if self.from == self.to {
            self.pending.extend(samples);
        } else {
            let chunk = AudioChunk::new(0, self.from.clone(), samples);
            let mapped = convert_to_mixer_channels(chunk, self.to.channel_count);
            for sample in mapped.audio_data {
                self.resampler.push_sample(sample);
            }
            while let Some(sample) = self.resampler.pop_sample() {
                self.pending.push_back(sample);
            }
        }
        if self.pending.len() > self.capacity_samples {
            let over = self.pending.len() - self.capacity_samples;
            self.pending.drain(..over);
            return over;
        }
        0
    }

    pub fn take_block(&mut self) -> Option<Vec<f32>> {
        if self.pending.len() < self.block_samples {
            return None;
        }
        Some(self.pending.drain(..self.block_samples).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::FormatConverter;
    use std::collections::VecDeque;

    use crate::audio::AudioFormat;
    use crate::audio::sample::{Resampler, ResamplerSpec};

    struct ModelPump {
        channels: usize,
        div: usize,
        staging: VecDeque<f32>,
        ready: VecDeque<f32>,
    }

    impl Resampler for ModelPump {
        fn push_sample(&mut self, sample: f32) {
            self.staging.push_back(sample);
            let group = self.channels.max(1) * self.div.max(1);
            while self.staging.len() >= group {
                let first = self.staging.drain(..group).next();
                self.ready.extend(first);
            }
        }

        fn pop_sample(&mut self) -> Option<f32> {
            self.ready.pop_front()
        }

        fn buffered(&self) -> usize {
            self.ready.len()
        }
    }

    impl From<ResamplerSpec> for ModelPump {
        fn from(spec: ResamplerSpec) -> Self {
            let div = spec.source_rate / spec.target_rate.max(1);
            ModelPump {
                channels: spec.source_channels,
                div: div.max(1) as usize,
                staging: VecDeque::new(),
                ready: VecDeque::new(),
            }
        }
    }

    #[test]
    fn passthrough_moves_samples() {
        let format = AudioFormat::new(2, 48000);
        let mut converter: FormatConverter<ModelPump> =
            FormatConverter::new(format.clone(), format.clone(), 4, 16);
        let samples = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(converter.feed(samples.clone()), 0);
        assert_eq!(converter.take_block(), Some(samples));
        assert_eq!(converter.take_block(), None);
    }

    #[test]
    fn channel_convert_changes_content_before_resampler() {
        let from = AudioFormat::new(2, 48000);
        let to = AudioFormat::new(1, 48000);
        let mut converter: FormatConverter<ModelPump> = FormatConverter::new(from, to, 2, 16);
        assert_eq!(converter.feed(vec![1.0, 3.0, 2.0, 4.0]), 0);
        assert_eq!(converter.take_block(), Some(vec![2.0, 3.0]));
    }

    #[test]
    fn ratio_pump_counts_span_feeds() {
        let from = AudioFormat::new(2, 48000);
        let to = AudioFormat::new(2, 24000);
        let mut converter: FormatConverter<ModelPump> = FormatConverter::new(from, to, 4, 64);
        for _ in 0..4 {
            assert_eq!(converter.feed(vec![0.5; 8]), 0);
        }
        let mut total = 0;
        while let Some(block) = converter.take_block() {
            total += block.len();
        }
        assert_eq!(total, 8);
    }

    #[test]
    fn capacity_bound_drops_oldest_and_reports() {
        let format = AudioFormat::new(1, 48000);
        let mut converter: FormatConverter<ModelPump> =
            FormatConverter::new(format.clone(), format, 2, 4);
        assert_eq!(converter.feed(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 2);
        assert_eq!(converter.pending_samples(), 4);
        assert_eq!(converter.take_block(), Some(vec![3.0, 4.0]));
        assert_eq!(converter.take_block(), Some(vec![5.0, 6.0]));
    }

    #[test]
    fn factory_derives_pump_channels_and_ratio() {
        let from = AudioFormat::new(2, 48000);
        let to = AudioFormat::new(1, 24000);
        let mut converter: FormatConverter<ModelPump> = FormatConverter::new(from, to, 2, 16);
        assert_eq!(converter.feed(vec![1.0, 3.0, 2.0, 4.0]), 0);
        assert_eq!(converter.feed(vec![1.0, 3.0, 2.0, 4.0]), 0);
        assert_eq!(converter.take_block(), Some(vec![2.0, 2.0]));
    }
}
