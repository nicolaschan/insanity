use crate::audio::AudioFormat;
use crate::audio::sample::SampleSource;
use futures_core::Stream;
use futures_util::stream;
use serde::{Deserialize, Serialize};

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

pub struct SampleChunker<S: SampleSource + Send> {
    source: S,
    frames: usize,
    next_sequence: u128,
}

impl<S: SampleSource + Send> SampleChunker<S> {
    pub fn new(source: S, frames: usize) -> Self {
        SampleChunker {
            source,
            frames,
            next_sequence: 0,
        }
    }

    pub fn into_stream(self) -> impl Stream<Item = AudioChunk> + Send
    where
        S: 'static,
    {
        stream::unfold(self, |mut chunker| async move {
            let chunk = chunker.next_chunk().await?;
            Some((chunk, chunker))
        })
    }

    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        let channels = self.source.format().channel_count as usize;
        if channels == 0 {
            return None;
        }
        let len = self.frames * channels;
        let mut audio_data = Vec::with_capacity(len);
        for _ in 0..len {
            audio_data.push(self.source.next().await?);
        }
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(
            sequence_number,
            self.source.format().clone(),
            audio_data,
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::SampleChunker;
    use crate::audio::AudioFormat;
    use crate::audio::sample::SampleSource;
    use futures_util::StreamExt;
    use std::collections::VecDeque;
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
        format: AudioFormat,
        next: f32,
    }

    impl SampleSource for Counting {
        async fn next(&mut self) -> Option<f32> {
            let value = self.next;
            self.next += 1.0;
            Some(value)
        }

        fn format(&self) -> &AudioFormat {
            &self.format
        }
    }

    #[test]
    fn sample_chunker_frames_and_counts_sequence() {
        let mut chunker = SampleChunker::new(
            Counting {
                next: 0.0,
                format: AudioFormat::new(2, 44100),
            },
            3,
        );
        let first = block_on(chunker.next_chunk()).expect("chunk");
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.format, AudioFormat::new(2, 44100));
        assert_eq!(first.audio_data, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        let second = block_on(chunker.next_chunk()).expect("chunk");
        assert_eq!(second.sequence_number, 1);
        assert_eq!(second.audio_data[0], 6.0);
    }

    struct Scripted(VecDeque<f32>, AudioFormat);

    impl SampleSource for Scripted {
        async fn next(&mut self) -> Option<f32> {
            self.0.pop_front()
        }

        fn format(&self) -> &AudioFormat {
            &self.1
        }
    }

    fn scripted(samples: Vec<f32>, channels: u16) -> Scripted {
        Scripted(VecDeque::from(samples), AudioFormat::new(channels, 48000))
    }

    #[test]
    fn exhausted_source_ends_stream() {
        let mut chunker = SampleChunker::new(scripted(vec![0.0; 4], 2), 2);
        assert!(block_on(chunker.next_chunk()).is_some());
        assert!(block_on(chunker.next_chunk()).is_none());
    }

    #[test]
    fn zero_channel_source_ends_stream() {
        let mut chunker = SampleChunker::new(scripted(vec![0.0; 4], 0), 2);
        assert!(block_on(chunker.next_chunk()).is_none());
    }

    #[test]
    fn into_stream_numbers_chunks_and_ends() {
        let mut chunks = Box::pin(SampleChunker::new(scripted(vec![0.5; 8], 2), 2).into_stream());
        let first = block_on(chunks.next()).expect("chunk");
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.audio_data, vec![0.5; 4]);
        let second = block_on(chunks.next()).expect("chunk");
        assert_eq!(second.sequence_number, 1);
        assert!(block_on(chunks.next()).is_none());
    }
}
