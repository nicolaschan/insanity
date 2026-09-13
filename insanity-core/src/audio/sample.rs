use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;
use futures_core::Stream;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, stream};
use std::pin::Pin;
use std::task::{Context, Poll};

pub trait SampleSource: Stream<Item = f32> {
    fn format(&self) -> &AudioFormat;

    fn into_chunks(self, frames: usize) -> impl Stream<Item = AudioChunk> + Send
    where
        Self: Sized + Send + 'static,
    {
        let format = self.format().clone();
        let len = frames * format.channel_count as usize;
        stream::unfold(
            (Box::pin(self), 0u128),
            move |(mut samples, sequence_number)| {
                let format = format.clone();
                async move {
                    if len == 0 {
                        return None;
                    }
                    let mut audio_data = Vec::with_capacity(len);
                    for _ in 0..len {
                        audio_data.push(samples.next().await?);
                    }
                    let chunk = AudioChunk::new(sequence_number, format, audio_data);
                    Some((chunk, (samples, sequence_number + 1)))
                }
            },
        )
    }
}

pub struct AudioStream {
    format: AudioFormat,
    samples: BoxStream<'static, f32>,
}

impl AudioStream {
    pub fn new(format: AudioFormat, samples: impl Stream<Item = f32> + Send + 'static) -> Self {
        AudioStream {
            format,
            samples: Box::pin(samples),
        }
    }
}

impl SampleSource for AudioStream {
    fn format(&self) -> &AudioFormat {
        &self.format
    }
}

impl Stream for AudioStream {
    type Item = f32;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<f32>> {
        self.samples.as_mut().poll_next(cx)
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
