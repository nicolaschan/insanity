use crate::audio::AudioFormat;
use crate::audio::sample::SampleSource;
use serde::{Deserialize, Serialize};
use std::future::Future;

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

pub trait ChunkSource {
    fn next_chunk(&mut self) -> impl Future<Output = Option<AudioChunk>> + Send;
}

pub struct SampleChunker<S: SampleSource + Send> {
    source: S,
    frames: usize,
    format: AudioFormat,
    next_sequence: u128,
}

impl<S: SampleSource + Send> SampleChunker<S> {
    pub fn new(source: S, frames: usize, format: AudioFormat) -> Self {
        SampleChunker {
            source,
            frames,
            format,
            next_sequence: 0,
        }
    }
}

impl<S: SampleSource + Send> ChunkSource for SampleChunker<S> {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        let len = self.frames * self.format.channel_count as usize;
        let mut audio_data = Vec::with_capacity(len);
        for _ in 0..len {
            audio_data.push(self.source.next().await?);
        }
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(
            sequence_number,
            self.format.clone(),
            audio_data,
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{ChunkSource, SampleChunker};
    use crate::audio::AudioFormat;
    use crate::audio::sample::SampleSource;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    pub(crate) struct Counting {
        next: f32,
    }

    impl SampleSource for Counting {
        async fn next(&mut self) -> Option<f32> {
            let value = self.next;
            self.next += 1.0;
            Some(value)
        }
    }

    #[test]
    fn sample_chunker_frames_and_counts_sequence() {
        let mut chunker = SampleChunker::new(Counting { next: 0.0 }, 3, AudioFormat::new(2, 44100));
        let first = block_on(chunker.next_chunk()).expect("chunk");
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.format, AudioFormat::new(2, 44100));
        assert_eq!(first.audio_data, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        let second = block_on(chunker.next_chunk()).expect("chunk");
        assert_eq!(second.sequence_number, 1);
        assert_eq!(second.audio_data[0], 6.0);
    }
}
