use std::collections::VecDeque;

use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;
use crate::audio::sample::{SampleSource, SyncSampleSource};

pub struct ChunkFlattener {
    sample_buffer: VecDeque<f32>,
    format: AudioFormat,
}

impl ChunkFlattener {
    pub fn new(format: AudioFormat) -> Self {
        ChunkFlattener {
            sample_buffer: VecDeque::new(),
            format,
        }
    }

    pub fn push_chunk(&mut self, chunk: AudioChunk) {
        self.sample_buffer.extend(chunk.audio_data);
    }

    pub fn len(&self) -> usize {
        self.sample_buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sample_buffer.is_empty()
    }
}

impl SampleSource for ChunkFlattener {
    async fn next(&mut self) -> Option<f32> {
        self.next_sync()
    }

    fn format(&self) -> AudioFormat {
        self.format.clone()
    }
}

impl SyncSampleSource for ChunkFlattener {
    fn next_sync(&mut self) -> Option<f32> {
        self.sample_buffer.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkFlattener;
    use crate::audio::AudioFormat;
    use crate::audio::chunk::AudioChunk;
    use crate::audio::sample::{SampleSource, SyncSampleSource};

    #[test]
    fn flattens_chunks_in_order() {
        let mut flattener = ChunkFlattener::new(AudioFormat::new(2, 48000));
        assert!(flattener.is_empty());
        flattener.push_chunk(AudioChunk::new(
            0,
            AudioFormat::new(2, 48000),
            vec![0.1, 0.2],
        ));
        flattener.push_chunk(AudioChunk::new(1, AudioFormat::new(2, 48000), vec![0.3]));
        assert_eq!(flattener.len(), 3);
        assert_eq!(flattener.next_sync(), Some(0.1));
        assert_eq!(flattener.next_sync(), Some(0.2));
        assert_eq!(flattener.next_sync(), Some(0.3));
        assert_eq!(flattener.next_sync(), None);
        assert!(flattener.is_empty());
    }

    #[test]
    fn empty_flattener_yields_none() {
        let mut flattener = ChunkFlattener::new(AudioFormat::new(1, 48000));
        assert_eq!(flattener.next_sync(), None);
        assert_eq!(flattener.format(), AudioFormat::new(1, 48000));
    }
}
