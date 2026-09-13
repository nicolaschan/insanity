use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;
use futures_core::Stream;
use futures_util::{StreamExt, future, stream};

pub trait SampleSource {
    type Samples: Stream<Item = f32> + Send + 'static;

    fn format(&self) -> &AudioFormat;

    fn into_samples(self) -> Self::Samples;

    fn map_samples<S>(self, f: impl FnOnce(Self::Samples) -> S) -> Sampled<S>
    where
        Self: Sized,
        S: Stream<Item = f32> + Send + 'static,
    {
        let format = self.format().clone();
        self.with_format(format, f)
    }

    fn with_format<S>(self, format: AudioFormat, f: impl FnOnce(Self::Samples) -> S) -> Sampled<S>
    where
        Self: Sized,
        S: Stream<Item = f32> + Send + 'static,
    {
        Sampled::new(format, f(self.into_samples()))
    }

    fn into_chunks(self, frames: usize) -> impl Stream<Item = AudioChunk> + Send + 'static
    where
        Self: Sized,
    {
        let format = self.format().clone();
        let len = frames * format.channel_count as usize;
        if len == 0 {
            return stream::empty().left_stream();
        }
        self.into_samples()
            .chunks(len)
            .take_while(move |audio_data| future::ready(audio_data.len() == len))
            .enumerate()
            .map(move |(sequence_number, audio_data)| {
                AudioChunk::new(sequence_number as u128, format.clone(), audio_data)
            })
            .right_stream()
    }
}

pub struct Sampled<S> {
    format: AudioFormat,
    samples: S,
}

impl<S> Sampled<S> {
    pub fn new(format: AudioFormat, samples: S) -> Self {
        Sampled { format, samples }
    }
}

impl<S: Stream<Item = f32> + Send + 'static> SampleSource for Sampled<S> {
    type Samples = S;

    fn format(&self) -> &AudioFormat {
        &self.format
    }

    fn into_samples(self) -> S {
        self.samples
    }
}

pub trait Resampler: Send {
    fn push_sample(&mut self, sample: f32);
    fn pop_sample(&mut self) -> Option<f32>;
    fn buffered(&self) -> usize;
    fn reset(&mut self) {}
    fn flush(&mut self) {}
    fn reconfigure(
        &mut self,
        _source_rate: u32,
        _source_channels: usize,
        _target_rate: u32,
        _block_frames: usize,
    ) {
    }
}
