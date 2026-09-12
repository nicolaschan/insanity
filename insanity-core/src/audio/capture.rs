use crate::audio::AudioFormat;
use crate::audio::chunk::{AudioChunk, ChunkSource, SampleChunker};
use crate::audio::codec::{AudioEncoder, EncodedChunk, FormatCache};
use crate::audio::sample::SampleSource;
use crate::audio::transform::ChunkTransform;

pub enum CaptureOutput {
    EndOfStream,
    Skipped,
    Encoded(EncodedChunk),
}

pub struct ChunkEncoder<E: AudioEncoder, F: FnMut(&AudioFormat) -> Option<E>> {
    encoder_cache: FormatCache<E, F>,
    frames: usize,
}

impl<E: AudioEncoder, F: FnMut(&AudioFormat) -> Option<E>> ChunkEncoder<E, F> {
    pub fn new(rebuild: F, frames: usize) -> Self {
        ChunkEncoder {
            encoder_cache: FormatCache::new(rebuild),
            frames,
        }
    }

    pub fn encode_chunk(&mut self, chunk: AudioChunk) -> Option<EncodedChunk> {
        if chunk.format.channel_count == 0 {
            return None;
        }
        let encoder = self.encoder_cache.ensure_current(&chunk.format)?;
        let frame = encoder.encode(&chunk)?;
        debug_assert_eq!(
            chunk.audio_data.len(),
            self.frames * chunk.format.channel_count as usize
        );
        Some(frame)
    }
}

pub struct Capture<
    R: SampleSource + Send,
    T: ChunkTransform,
    E: AudioEncoder,
    F: FnMut(&AudioFormat) -> Option<E>,
> {
    chunker: SampleChunker<R>,
    transform: T,
    encoder: ChunkEncoder<E, F>,
}

impl<
    R: SampleSource + Send,
    T: ChunkTransform,
    E: AudioEncoder,
    F: FnMut(&AudioFormat) -> Option<E>,
> Capture<R, T, E, F>
{
    pub fn new(
        resampled: R,
        stream_format: AudioFormat,
        frames: usize,
        transform: T,
        rebuild: F,
    ) -> Self {
        Capture {
            chunker: SampleChunker::new(resampled, frames, stream_format),
            transform,
            encoder: ChunkEncoder::new(rebuild, frames),
        }
    }

    pub async fn next_output(&mut self) -> CaptureOutput {
        let Some(chunk) = self.chunker.next_chunk().await else {
            return CaptureOutput::EndOfStream;
        };
        if chunk.format.channel_count == 0 {
            return CaptureOutput::EndOfStream;
        }
        let Some(chunk) = self.transform.transform(chunk) else {
            return CaptureOutput::Skipped;
        };
        match self.encoder.encode_chunk(chunk) {
            Some(frame) => CaptureOutput::Encoded(frame),
            None => CaptureOutput::Skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Capture, CaptureOutput, ChunkEncoder};
    use crate::audio::AudioFormat;
    use crate::audio::chunk::{AudioChunk, ChunkSource, tests::block_on};
    use crate::audio::codec::{AudioCodec, AudioEncoder, EncodedChunk};
    use crate::audio::sample::SampleSource;
    use crate::audio::transform::{ChannelMap, ChunkTransform, Mute};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scripted(VecDeque<f32>);

    impl SampleSource for Scripted {
        async fn next(&mut self) -> Option<f32> {
            self.0.pop_front()
        }
    }

    struct Chunks(VecDeque<AudioChunk>);

    impl ChunkSource for Chunks {
        async fn next_chunk(&mut self) -> Option<AudioChunk> {
            self.0.pop_front()
        }
    }

    struct TagEncoder;

    impl AudioEncoder for TagEncoder {
        fn encode(&mut self, chunk: &AudioChunk) -> Option<EncodedChunk> {
            Some(EncodedChunk {
                sequence_number: chunk.sequence_number,
                codec: AudioCodec::Raw,
                payload: Vec::new(),
                format: chunk.format.clone(),
            })
        }
    }

    fn chunk(seq: u128, format: AudioFormat, data: Vec<f32>) -> AudioChunk {
        AudioChunk::new(seq, format, data)
    }

    fn capture<T, F>(
        source: Scripted,
        format: AudioFormat,
        transform: T,
        rebuild: F,
    ) -> Capture<Scripted, T, TagEncoder, F>
    where
        T: ChunkTransform,
        F: FnMut(&AudioFormat) -> Option<TagEncoder>,
    {
        Capture::new(source, format, 2, transform, rebuild)
    }

    #[test]
    fn encodes_and_advances_sequence() {
        let format = AudioFormat::new(2, 48000);
        let mut cap = capture(Scripted(VecDeque::from(vec![0.0; 8])), format, (), |_| {
            Some(TagEncoder)
        });
        for seq in 0..2 {
            let frame = match block_on(cap.next_output()) {
                CaptureOutput::Encoded(frame) => frame,
                _ => panic!("expected encoded"),
            };
            assert_eq!(frame.sequence_number, seq);
        }
    }

    #[test]
    fn muted_chunks_skip_but_consume_sequence() {
        let format = AudioFormat::new(2, 48000);
        let (mute, control) = Mute::shared(false);
        let mut cap = capture(Scripted(VecDeque::from(vec![0.0; 8])), format, mute, |_| {
            Some(TagEncoder)
        });
        control.set(true);
        assert!(matches!(
            block_on(cap.next_output()),
            CaptureOutput::Skipped
        ));
        control.set(false);
        let frame = match block_on(cap.next_output()) {
            CaptureOutput::Encoded(frame) => frame,
            _ => panic!("expected encoded after unmute"),
        };
        assert_eq!(frame.sequence_number, 1);
    }

    #[test]
    fn creates_encoder_once_and_reuses_it() {
        let format = AudioFormat::new(2, 48000);
        let rebuilds = AtomicUsize::new(0);
        let mut cap = capture(
            Scripted(VecDeque::from(vec![0.0; 8])),
            format,
            (),
            |_: &AudioFormat| {
                rebuilds.fetch_add(1, Ordering::Relaxed);
                Some(TagEncoder)
            },
        );
        assert!(matches!(
            block_on(cap.next_output()),
            CaptureOutput::Encoded(_)
        ));
        assert!(matches!(
            block_on(cap.next_output()),
            CaptureOutput::Encoded(_)
        ));
        assert_eq!(rebuilds.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_rebuild_skips_chunk() {
        let format = AudioFormat::new(2, 48000);
        let mut cap = capture(Scripted(VecDeque::from(vec![0.0; 4])), format, (), |_| {
            None::<TagEncoder>
        });
        assert!(matches!(
            block_on(cap.next_output()),
            CaptureOutput::Skipped
        ));
    }

    #[test]
    fn exhausted_source_ends_stream() {
        let format = AudioFormat::new(2, 48000);
        let mut cap = capture(Scripted(VecDeque::from(vec![0.0; 4])), format, (), |_| {
            Some(TagEncoder)
        });
        assert!(matches!(
            block_on(cap.next_output()),
            CaptureOutput::Encoded(_)
        ));
        assert!(matches!(
            block_on(cap.next_output()),
            CaptureOutput::EndOfStream
        ));
    }

    #[test]
    fn zero_channel_stream_ends_instead_of_spinning() {
        let mut cap = capture(
            Scripted(VecDeque::from(vec![0.0; 4])),
            AudioFormat::new(0, 48000),
            (),
            |_| Some(TagEncoder),
        );
        assert!(matches!(
            block_on(cap.next_output()),
            CaptureOutput::EndOfStream
        ));
    }

    #[test]
    fn tracks_format_switches_with_continuous_sequence() {
        let mono = AudioFormat::new(1, 48000);
        let stereo = AudioFormat::new(2, 48000);
        let rebuilds = AtomicUsize::new(0);
        let mut encoder = ChunkEncoder::new(
            |_: &AudioFormat| {
                rebuilds.fetch_add(1, Ordering::Relaxed);
                Some(TagEncoder)
            },
            480,
        );
        let mut transform = ChannelMap::capped(2);
        let mut chunks = Chunks(VecDeque::from(vec![
            chunk(0, mono.clone(), vec![0.1; 480]),
            chunk(1, mono.clone(), vec![0.1; 480]),
            chunk(2, mono.clone(), vec![0.1; 480]),
            chunk(3, stereo.clone(), vec![0.2; 960]),
            chunk(4, stereo.clone(), vec![0.2; 960]),
            chunk(5, stereo.clone(), vec![0.2; 960]),
        ]));
        let mut formats = Vec::new();
        let mut sequences = Vec::new();
        while let Some(input) = block_on(chunks.next_chunk()) {
            let Some(converted) = transform.transform(input) else {
                panic!("expected converted");
            };
            match encoder.encode_chunk(converted) {
                Some(frame) => {
                    formats.push(frame.format.clone());
                    sequences.push(frame.sequence_number);
                }
                None => panic!("expected encoded"),
            }
        }
        assert_eq!(
            formats,
            vec![
                mono.clone(),
                mono.clone(),
                mono.clone(),
                stereo.clone(),
                stereo.clone(),
                stereo.clone()
            ]
        );
        assert_eq!(sequences, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(rebuilds.load(Ordering::Relaxed), 2);
    }
}
