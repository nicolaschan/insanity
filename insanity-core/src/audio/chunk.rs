use crate::audio::AudioFormat;
use crate::audio::sample::SampleSource;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
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

    pub fn frames(&self) -> usize {
        match self.format.channel_count {
            0 => 0,
            channels => self.audio_data.len() / channels as usize,
        }
    }
}

pub trait ChunkSource {
    fn next_chunk(&mut self) -> impl Future<Output = Option<AudioChunk>> + Send;
}

pub trait ChunkSink {
    fn push_chunk(&mut self, chunk: AudioChunk);
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

/// Re-cuts a chunk stream into exactly `frames` frames per chunk.
pub struct Rechunker<S> {
    source: S,
    frames: usize,
    format: Option<AudioFormat>,
    pending: VecDeque<f32>,
    next_sequence: u128,
}

impl<S: ChunkSource + Send> Rechunker<S> {
    pub fn new(source: S, frames: usize) -> Self {
        Rechunker {
            source,
            frames,
            format: None,
            pending: VecDeque::new(),
            next_sequence: 0,
        }
    }

    fn take_ready(&mut self) -> Option<AudioChunk> {
        let format = self.format.as_ref()?;
        let len = self.frames * format.channel_count as usize;
        if len == 0 || self.pending.len() < len {
            return None;
        }
        let audio_data = self.pending.drain(..len).collect();
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(sequence_number, format.clone(), audio_data))
    }
}

impl<S: ChunkSource + Send> ChunkSource for Rechunker<S> {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        loop {
            if let Some(chunk) = self.take_ready() {
                return Some(chunk);
            }
            let chunk = self.source.next_chunk().await?;
            if self.format.as_ref() != Some(&chunk.format) {
                self.pending.clear();
                self.format = Some(chunk.format);
            }
            self.pending.extend(chunk.audio_data);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{AudioChunk, ChunkSource, Rechunker, SampleChunker};
    use crate::audio::AudioFormat;
    use crate::audio::sample::SampleSource;
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
        next: f32,
    }

    impl SampleSource for Counting {
        async fn next(&mut self) -> Option<f32> {
            let value = self.next;
            self.next += 1.0;
            Some(value)
        }
    }

    pub(crate) struct Scripted(pub VecDeque<AudioChunk>);

    impl ChunkSource for Scripted {
        async fn next_chunk(&mut self) -> Option<AudioChunk> {
            self.0.pop_front()
        }
    }

    #[test]
    fn frames_divides_by_channel_count() {
        let chunk = AudioChunk::new(0, AudioFormat::new(2, 48000), vec![0.0; 10]);
        assert_eq!(chunk.frames(), 5);
        let chunk = AudioChunk::new(0, AudioFormat::new(0, 48000), vec![0.0; 10]);
        assert_eq!(chunk.frames(), 0);
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

    #[test]
    fn rechunker_recuts_to_fixed_frames() {
        let format = AudioFormat::new(2, 48000);
        let source = Scripted(VecDeque::from(vec![
            AudioChunk::new(0, format.clone(), vec![0.0, 1.0, 2.0]),
            AudioChunk::new(1, format.clone(), vec![3.0, 4.0, 5.0, 6.0, 7.0]),
        ]));
        let mut rechunker = Rechunker::new(source, 2);
        let first = block_on(rechunker.next_chunk()).expect("chunk");
        assert_eq!(first.audio_data, vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(first.format, format);
        assert_eq!(first.sequence_number, 0);
        let second = block_on(rechunker.next_chunk()).expect("chunk");
        assert_eq!(second.audio_data, vec![4.0, 5.0, 6.0, 7.0]);
        assert_eq!(second.sequence_number, 1);
        assert!(block_on(rechunker.next_chunk()).is_none());
    }

    #[test]
    fn rechunker_drops_partial_data_on_format_change() {
        let mono = AudioFormat::new(1, 48000);
        let stereo = AudioFormat::new(2, 48000);
        let source = Scripted(VecDeque::from(vec![
            AudioChunk::new(0, mono, vec![9.0]),
            AudioChunk::new(1, stereo.clone(), vec![0.0, 1.0, 2.0, 3.0]),
        ]));
        let mut rechunker = Rechunker::new(source, 2);
        let chunk = block_on(rechunker.next_chunk()).expect("chunk");
        assert_eq!(chunk.format, stereo);
        assert_eq!(chunk.audio_data, vec![0.0, 1.0, 2.0, 3.0]);
    }
}
