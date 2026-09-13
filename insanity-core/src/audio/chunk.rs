use crate::audio::AudioFormat;
use crate::audio::sample::SampleSource;
use crate::audio::transform::ChunkTransform;
use futures_core::Stream;
use futures_util::{StreamExt, stream};
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

pub fn chunk_samples<S: SampleSource + Send + 'static>(
    source: S,
    frames: usize,
) -> impl Stream<Item = AudioChunk> + Send {
    stream::unfold(
        (source, 0u128),
        move |(mut source, sequence_number)| async move {
            let channels = source.format().channel_count as usize;
            if channels == 0 {
                return None;
            }
            let len = frames * channels;
            let mut audio_data = Vec::with_capacity(len);
            for _ in 0..len {
                audio_data.push(source.next().await?);
            }
            let chunk = AudioChunk::new(sequence_number, source.format().clone(), audio_data);
            Some((chunk, (source, sequence_number + 1)))
        },
    )
}

pub trait ChunkStreamExt: Stream<Item = AudioChunk> + Sized {
    fn transform<T: ChunkTransform>(self, mut transform: T) -> impl Stream<Item = AudioChunk> + Send
    where
        Self: Send,
    {
        self.map(move |chunk| transform.transform(chunk))
    }
}

impl<S: Stream<Item = AudioChunk>> ChunkStreamExt for S {}

#[cfg(test)]
pub(crate) mod tests {
    use super::{ChunkStreamExt, chunk_samples};
    use crate::audio::AudioFormat;
    use crate::audio::sample::SampleSource;
    use crate::audio::transform::Mute;
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
        let mut chunker = pin!(chunk_samples(
            Counting {
                next: 0.0,
                format: AudioFormat::new(2, 44100),
            },
            3,
        ));
        let first = block_on(chunker.next()).expect("chunk");
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.format, AudioFormat::new(2, 44100));
        assert_eq!(first.audio_data, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        let second = block_on(chunker.next()).expect("chunk");
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
        let mut chunker = pin!(chunk_samples(scripted(vec![0.0; 4], 2), 2));
        assert!(block_on(chunker.next()).is_some());
        assert!(block_on(chunker.next()).is_none());
    }

    #[test]
    fn zero_channel_source_ends_stream() {
        let mut chunker = pin!(chunk_samples(scripted(vec![0.0; 4], 0), 2));
        assert!(block_on(chunker.next()).is_none());
    }

    #[test]
    fn transform_applies_and_keeps_sequence() {
        let (mute, control) = Mute::shared(true);
        let mut chunks = pin!(chunk_samples(scripted(vec![0.5; 8], 2), 2).transform(mute));
        let muted = block_on(chunks.next()).expect("chunk");
        assert_eq!(muted.sequence_number, 0);
        assert_eq!(muted.audio_data, vec![0.0; 4]);
        control.set(false);
        let live = block_on(chunks.next()).expect("chunk");
        assert_eq!(live.sequence_number, 1);
        assert_eq!(live.audio_data, vec![0.5; 4]);
        assert!(block_on(chunks.next()).is_none());
    }
}
