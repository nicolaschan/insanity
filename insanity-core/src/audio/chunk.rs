use crate::audio::AudioFormat;
use crate::audio::sample::AudioStream;
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

impl AudioStream {
    pub fn into_chunks(self, frames: usize) -> impl Stream<Item = AudioChunk> + Send {
        let format = self.format().clone();
        let len = frames * format.channel_count as usize;
        stream::unfold((self, 0u128), move |(mut samples, sequence_number)| {
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::AudioChunk;
    use crate::audio::AudioFormat;
    use crate::audio::sample::AudioStream;
    use futures_util::future::FutureExt;
    use futures_util::stream::{self, BoxStream, StreamExt};

    fn chunker(samples: Vec<f32>, channels: u16, frames: usize) -> BoxStream<'static, AudioChunk> {
        AudioStream::new(AudioFormat::new(channels, 44100), stream::iter(samples))
            .into_chunks(frames)
            .boxed()
    }

    fn next(chunker: &mut BoxStream<'static, AudioChunk>) -> Option<AudioChunk> {
        chunker.next().now_or_never().flatten()
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
