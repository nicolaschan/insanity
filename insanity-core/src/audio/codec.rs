use serde::{Deserialize, Serialize};

use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodedChunk {
    pub sequence_number: u128,
    pub codec: AudioCodec,
    pub payload: Vec<u8>,
    pub format: AudioFormat,
}

pub trait AudioEncoder: Send {
    fn encode(&mut self, chunk: &AudioChunk) -> Option<EncodedChunk>;
}

pub trait AudioDecoder: Send {
    fn decode(&mut self, frame: &EncodedChunk) -> Option<AudioChunk>;
}

pub(crate) struct FormatCache<C, F: FnMut(&AudioFormat) -> Option<C>> {
    codec: Option<C>,
    format: Option<AudioFormat>,
    rebuild: F,
}

impl<C, F: FnMut(&AudioFormat) -> Option<C>> FormatCache<C, F> {
    pub(crate) fn new(rebuild: F) -> Self {
        FormatCache {
            codec: None,
            format: None,
            rebuild,
        }
    }

    pub(crate) fn format(&self) -> Option<&AudioFormat> {
        self.format.as_ref()
    }

    pub(crate) fn ensure_current(&mut self, format: &AudioFormat) -> Option<&mut C> {
        if self.format.as_ref() != Some(format) {
            let codec = (self.rebuild)(format)?;
            self.codec = Some(codec);
            self.format = Some(format.clone());
        }
        self.codec.as_mut()
    }

    pub(crate) fn reset(&mut self) {
        let Some(format) = self.format.clone() else {
            return;
        };
        self.codec = (self.rebuild)(&format);
    }
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

#[cfg(test)]
mod tests {
    use super::{AudioCodec, AudioEncoder, ChunkEncoder, EncodedChunk};
    use crate::audio::AudioFormat;
    use crate::audio::chunk::AudioChunk;
    use crate::audio::transform::{ChannelMap, ChunkTransform};
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn creates_encoder_once_and_reuses_it() {
        let format = AudioFormat::new(2, 48000);
        let rebuilds = AtomicUsize::new(0);
        let mut encoder = ChunkEncoder::new(
            |_: &AudioFormat| {
                rebuilds.fetch_add(1, Ordering::Relaxed);
                Some(TagEncoder)
            },
            2,
        );
        for seq in 0..2 {
            let frame = encoder
                .encode_chunk(chunk(seq, format.clone(), vec![0.0; 4]))
                .expect("encoded");
            assert_eq!(frame.sequence_number, seq);
        }
        assert_eq!(rebuilds.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_rebuild_skips_chunk() {
        let mut encoder = ChunkEncoder::new(|_: &AudioFormat| None::<TagEncoder>, 2);
        let chunk = chunk(0, AudioFormat::new(2, 48000), vec![0.0; 4]);
        assert!(encoder.encode_chunk(chunk).is_none());
    }

    #[test]
    fn zero_channel_chunk_is_skipped() {
        let mut encoder = ChunkEncoder::new(|_: &AudioFormat| Some(TagEncoder), 2);
        let chunk = chunk(0, AudioFormat::new(0, 48000), Vec::new());
        assert!(encoder.encode_chunk(chunk).is_none());
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
        let inputs = vec![
            chunk(0, mono.clone(), vec![0.1; 480]),
            chunk(1, mono.clone(), vec![0.1; 480]),
            chunk(2, mono.clone(), vec![0.1; 480]),
            chunk(3, stereo.clone(), vec![0.2; 960]),
            chunk(4, stereo.clone(), vec![0.2; 960]),
            chunk(5, stereo.clone(), vec![0.2; 960]),
        ];
        let mut formats = Vec::new();
        let mut sequences = Vec::new();
        for input in inputs {
            let converted = transform.transform(input);
            let frame = encoder.encode_chunk(converted).expect("encoded");
            formats.push(frame.format);
            sequences.push(frame.sequence_number);
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
