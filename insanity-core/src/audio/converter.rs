use std::collections::VecDeque;

use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;
use crate::audio::sample::{Resampler, ResamplerSpec, SampleSource, SyncSampleSource};
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
        debug_assert!(to.channel_count > 0);
        let channels = usize::from(to.channel_count);
        let resampler = R::from(ResamplerSpec {
            source_rate: from.sample_rate,
            source_channels: channels,
            target_rate: to.sample_rate,
            block_frames: block_samples.checked_div(channels).unwrap_or(0),
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
        let mut dropped = 0;
        if self.from == self.to {
            self.pending.extend(samples);
        } else {
            let src_channels = usize::from(self.from.channel_count);
            if src_channels > 0 {
                dropped += samples.len() % src_channels;
            }
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
            dropped += over;
        }
        dropped
    }

    pub fn take_block(&mut self) -> Option<Vec<f32>> {
        if self.block_samples == 0 || self.pending.len() < self.block_samples {
            return None;
        }
        Some(self.pending.drain(..self.block_samples).collect())
    }
}

pub struct ResampledSource<S: SampleSource, R: Resampler> {
    delegate: S,
    resampler: R,
    out_format: AudioFormat,
    drained: bool,
}

impl<S: SampleSource + Send, R: Resampler + From<ResamplerSpec>> ResampledSource<S, R> {
    pub fn new(delegate: S, target_rate: u32, chunk_frames: usize) -> Self {
        let source = delegate.format().clone();
        let out_format = AudioFormat::new(source.channel_count, target_rate);
        let resampler = R::from(ResamplerSpec {
            source_rate: source.sample_rate,
            source_channels: usize::from(source.channel_count),
            target_rate,
            block_frames: chunk_frames,
        });
        Self {
            delegate,
            resampler,
            out_format,
            drained: false,
        }
    }
}

impl<S: SampleSource + Send, R: Resampler> ResampledSource<S, R> {
    fn drive(&mut self, input: Option<f32>) -> Option<f32> {
        match input {
            Some(sample) => {
                self.resampler.push_sample(sample);
                self.resampler.pop_sample()
            }
            None => {
                self.drained = true;
                self.resampler.pop_sample()
            }
        }
    }
}

impl<S: SampleSource + Send, R: Resampler> SampleSource for ResampledSource<S, R> {
    fn format(&self) -> &AudioFormat {
        &self.out_format
    }

    async fn next(&mut self) -> Option<f32> {
        if let Some(sample) = self.resampler.pop_sample() {
            return Some(sample);
        }
        if self.drained {
            return None;
        }
        loop {
            let input = self.delegate.next().await;
            let out = self.drive(input);
            if out.is_some() || self.drained {
                return out;
            }
        }
    }
}

impl<S: SyncSampleSource + Send, R: Resampler> SyncSampleSource for ResampledSource<S, R> {
    fn next_sync(&mut self) -> Option<f32> {
        if let Some(sample) = self.resampler.pop_sample() {
            return Some(sample);
        }
        if self.drained {
            return None;
        }
        loop {
            let input = self.delegate.next_sync();
            let out = self.drive(input);
            if out.is_some() || self.drained {
                return out;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FormatConverter, ResampledSource};
    use std::collections::VecDeque;

    use crate::audio::AudioFormat;
    use crate::audio::sample::{Resampler, ResamplerSpec, SampleSource, SyncSampleSource};

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

    struct VecSource {
        format: AudioFormat,
        data: VecDeque<f32>,
    }

    impl VecSource {
        fn new(channels: u16, rate: u32, data: Vec<f32>) -> Self {
            VecSource {
                format: AudioFormat::new(channels, rate),
                data: data.into(),
            }
        }
    }

    impl SampleSource for VecSource {
        fn format(&self) -> &AudioFormat {
            &self.format
        }

        async fn next(&mut self) -> Option<f32> {
            self.data.pop_front()
        }
    }

    impl SyncSampleSource for VecSource {
        fn next_sync(&mut self) -> Option<f32> {
            self.data.pop_front()
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
    fn zero_block_takes_nothing() {
        let format = AudioFormat::new(1, 48000);
        let mut converter: FormatConverter<ModelPump> =
            FormatConverter::new(format.clone(), format, 0, 4);
        assert_eq!(converter.feed(vec![1.0, 2.0]), 0);
        assert_eq!(converter.take_block(), None);
    }

    #[test]
    fn ragged_feed_counts_truncation() {
        let from = AudioFormat::new(2, 48000);
        let to = AudioFormat::new(1, 48000);
        let mut converter: FormatConverter<ModelPump> = FormatConverter::new(from, to, 1, 16);
        assert_eq!(converter.feed(vec![1.0, 3.0, 2.0]), 1);
        assert_eq!(converter.take_block(), Some(vec![2.0]));
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

    #[test]
    fn resampled_source_emits_processed_output_at_end_of_stream() {
        let delegate = VecSource::new(1, 48000, vec![1.0, 2.0, 3.0, 4.0]);
        let mut source: ResampledSource<_, ModelPump> = ResampledSource::new(delegate, 24000, 480);
        assert_eq!(source.format(), &AudioFormat::new(1, 24000));
        let out = crate::audio::chunk::tests::block_on(async {
            let mut v = Vec::new();
            while let Some(s) = source.next().await {
                v.push(s);
            }
            v
        });
        assert_eq!(out, vec![1.0, 3.0]);
    }

    #[test]
    fn resampled_source_derives_channels_from_delegate() {
        let delegate = VecSource::new(2, 44100, vec![1.0, 2.0]);
        let source: ResampledSource<_, ModelPump> = ResampledSource::new(delegate, 48000, 480);
        assert_eq!(source.format(), &AudioFormat::new(2, 48000));
    }

    #[test]
    fn resampled_source_sync_matches_async() {
        let data = vec![4.0, 5.0, 6.0, 7.0];
        let mut async_source: ResampledSource<_, ModelPump> =
            ResampledSource::new(VecSource::new(1, 48000, data.clone()), 48000, 480);
        let async_out = crate::audio::chunk::tests::block_on(async {
            let mut v = Vec::new();
            while let Some(s) = async_source.next().await {
                v.push(s);
            }
            v
        });
        let mut sync_source: ResampledSource<_, ModelPump> =
            ResampledSource::new(VecSource::new(1, 48000, data), 48000, 480);
        let mut sync_out = Vec::new();
        while let Some(s) = sync_source.next_sync() {
            sync_out.push(s);
        }
        assert_eq!(async_out, sync_out);
    }
}
