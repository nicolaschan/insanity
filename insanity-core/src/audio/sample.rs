use crate::audio::AudioFormat;
use futures_core::Stream;
use futures_util::stream::BoxStream;
use std::pin::Pin;
use std::task::{Context, Poll};

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

    pub fn format(&self) -> &AudioFormat {
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
