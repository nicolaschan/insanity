use crate::audio::AudioFormat;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub sequence_number: u128,
    pub format: AudioFormat,
    pub audio_data: Vec<f32>,
}

impl AudioChunk {
    pub fn new(sequence_number: u128, format: AudioFormat, audio_data: Vec<f32>) -> AudioChunk {
        AudioChunk {
            sequence_number,
            format,
            audio_data,
        }
    }
}

pub struct SampleChunker<S> {
    source: S,
    format: AudioFormat,
    frames: usize,
    buffer: Vec<f32>,
    next_sequence: u128,
}

impl<S> SampleChunker<S> {
    pub fn new(source: S, format: AudioFormat, frames: usize) -> Self {
        let buffer = Vec::with_capacity(frames * format.channel_count as usize);
        SampleChunker {
            source,
            format,
            frames,
            buffer,
            next_sequence: 0,
        }
    }

    fn chunk_len(&self) -> usize {
        self.frames * self.format.channel_count as usize
    }
}

impl<S: Stream<Item = f32> + Unpin> Stream for SampleChunker<S> {
    type Item = AudioChunk;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AudioChunk>> {
        let this = self.get_mut();
        let len = this.chunk_len();
        if len == 0 {
            return Poll::Ready(None);
        }
        while this.buffer.len() < len {
            match Pin::new(&mut this.source).poll_next(cx) {
                Poll::Ready(Some(sample)) => this.buffer.push(sample),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
        let audio_data = std::mem::replace(&mut this.buffer, Vec::with_capacity(len));
        let sequence_number = this.next_sequence;
        this.next_sequence += 1;
        Poll::Ready(Some(AudioChunk::new(
            sequence_number,
            this.format.clone(),
            audio_data,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::SampleChunker;
    use crate::audio::AudioFormat;
    use futures_core::Stream;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn next<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(out) = Pin::new(&mut *stream).poll_next(&mut cx) {
                return out;
            }
        }
    }

    struct Scripted(VecDeque<f32>);

    impl Stream for Scripted {
        type Item = f32;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<f32>> {
            Poll::Ready(self.get_mut().0.pop_front())
        }
    }

    fn chunker(samples: Vec<f32>, channels: u16, frames: usize) -> SampleChunker<Scripted> {
        SampleChunker::new(
            Scripted(VecDeque::from(samples)),
            AudioFormat::new(channels, 44100),
            frames,
        )
    }

    #[test]
    fn frames_and_counts_sequence() {
        let mut chunker = chunker((0..12).map(|v| v as f32).collect(), 2, 3);
        let first = next(&mut chunker).expect("chunk");
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.format, AudioFormat::new(2, 44100));
        assert_eq!(first.audio_data, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        let second = next(&mut chunker).expect("chunk");
        assert_eq!(second.sequence_number, 1);
        assert_eq!(second.audio_data[0], 6.0);
    }

    #[test]
    fn partial_tail_ends_stream() {
        let mut chunker = chunker(vec![0.0; 6], 2, 2);
        assert!(next(&mut chunker).is_some());
        assert!(next(&mut chunker).is_none());
    }

    #[test]
    fn zero_channel_source_ends_stream() {
        let mut chunker = chunker(vec![0.0; 4], 0, 2);
        assert!(next(&mut chunker).is_none());
    }
}
