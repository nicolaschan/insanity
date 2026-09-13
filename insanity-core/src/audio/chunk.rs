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
mod tests {
    use super::{AudioChunk, ChunkStreamExt, chunk_samples};
    use crate::audio::AudioFormat;
    use crate::audio::sample::SampleSource;
    use crate::audio::transform::Mute;
    use futures_util::{FutureExt, Stream, StreamExt};
    use std::collections::VecDeque;

    fn collect(chunks: impl Stream<Item = AudioChunk>) -> Vec<AudioChunk> {
        chunks.collect().now_or_never().expect("sources are ready")
    }

    struct Counting {
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
        let counting = Counting {
            next: 0.0,
            format: AudioFormat::new(2, 44100),
        };
        let chunks = collect(chunk_samples(counting, 3).take(2));
        assert_eq!(chunks[0].sequence_number, 0);
        assert_eq!(chunks[0].format, AudioFormat::new(2, 44100));
        assert_eq!(chunks[0].audio_data, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(chunks[1].sequence_number, 1);
        assert_eq!(chunks[1].audio_data[0], 6.0);
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
    fn partial_tail_ends_stream() {
        let chunks = collect(chunk_samples(scripted(vec![0.0; 6], 2), 2));
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn zero_channel_source_ends_stream() {
        let chunks = collect(chunk_samples(scripted(vec![0.0; 4], 0), 2));
        assert!(chunks.is_empty());
    }

    #[test]
    fn transform_applies_and_keeps_sequence() {
        let (mute, control) = Mute::shared(true);
        let chunks = collect(
            chunk_samples(scripted(vec![0.5; 8], 2), 2)
                .transform(mute)
                .inspect(|_| control.set(false)),
        );
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].sequence_number, 0);
        assert_eq!(chunks[0].audio_data, vec![0.0; 4]);
        assert_eq!(chunks[1].sequence_number, 1);
        assert_eq!(chunks[1].audio_data, vec![0.5; 4]);
    }
}
