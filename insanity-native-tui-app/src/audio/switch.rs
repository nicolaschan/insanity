use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub struct SwapRequest<T> {
    pub payload: T,
    pub on_adopt: Box<dyn FnOnce() + Send>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    FollowDefault,
    Explicit(String),
}

pub struct SwitchingChunkSource<T> {
    source: Option<T>,
    swap_tx: UnboundedSender<SwapRequest<T>>,
    swap_rx: UnboundedReceiver<SwapRequest<T>>,
    next_seq: u128,
}

impl<T: ChunkSource + Send> SwitchingChunkSource<T> {
    pub fn new(source: T) -> Self {
        let (swap_tx, swap_rx) = unbounded_channel();
        Self {
            source: Some(source),
            swap_tx,
            swap_rx,
            next_seq: 0,
        }
    }

    pub fn switcher(&self) -> UnboundedSender<SwapRequest<T>> {
        self.swap_tx.clone()
    }
}

impl<T: ChunkSource + Send> ChunkSource for SwitchingChunkSource<T> {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        loop {
            let Some(source) = self.source.as_mut() else {
                match self.swap_rx.recv().await {
                    Some(request) => {
                        self.source = Some(request.payload);
                        (request.on_adopt)();
                    }
                    None => self.source = None,
                }
                continue;
            };
            tokio::select! {
                biased;
                swapped = self.swap_rx.recv() => {
                    match swapped {
                        Some(request) => {
                            self.source = Some(request.payload);
                            (request.on_adopt)();
                        }
                        None => self.source = None,
                    }
                }
                chunk = source.next_chunk() => match chunk {
                    Some(mut c) => {
                        let seq = self.next_seq;
                        self.next_seq += 1;
                        c.sequence_number = seq;
                        return Some(c);
                    }
                    None => self.source = None,
                },
            }
        }
    }
}

pub struct SilenceChunkSource {
    format: AudioFormat,
    frames: usize,
    next_seq: u128,
}

impl SilenceChunkSource {
    pub fn new(format: AudioFormat, frames: usize) -> Self {
        Self {
            format,
            frames,
            next_seq: 0,
        }
    }
}

impl ChunkSource for SilenceChunkSource {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let samples = vec![0.0; self.frames * usize::from(self.format.channel_count)];
        Some(AudioChunk::new(seq, self.format.clone(), samples))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{SilenceChunkSource, SwapRequest, SwitchingChunkSource};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::{AudioChunk, ChunkSource};

    struct Constant {
        format: AudioFormat,
        value: f32,
        remaining: usize,
    }

    impl Constant {
        fn new(channels: u16, value: f32, remaining: usize) -> Self {
            Constant {
                format: AudioFormat::new(channels, 48000),
                value,
                remaining,
            }
        }
    }

    impl ChunkSource for Constant {
        async fn next_chunk(&mut self) -> Option<AudioChunk> {
            if self.remaining == 0 {
                return None;
            }
            self.remaining -= 1;
            Some(AudioChunk::new(0, self.format.clone(), vec![self.value; 4]))
        }
    }

    #[tokio::test]
    async fn swap_takes_effect_before_next_chunk_and_carries_format() {
        let mut source = SwitchingChunkSource::new(Constant::new(2, 1.0, 10));
        let switcher = source.switcher();
        let first = source.next_chunk().await.unwrap();
        assert_eq!(first.audio_data, vec![1.0; 4]);
        assert_eq!(first.format.channel_count, 2);
        switcher
            .send(SwapRequest {
                payload: Constant::new(1, 2.0, 10),
                on_adopt: Box::new(|| {}),
            })
            .unwrap();
        let second = source.next_chunk().await.unwrap();
        assert_eq!(second.audio_data, vec![2.0; 4]);
        assert_eq!(second.format.channel_count, 1);
    }

    #[tokio::test]
    async fn exhausted_source_parks_until_swapped() {
        let mut source = SwitchingChunkSource::new(Constant::new(2, 1.0, 1));
        let switcher = source.switcher();
        assert!(source.next_chunk().await.is_some());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            switcher
                .send(SwapRequest {
                    payload: Constant::new(2, 3.0, 1),
                    on_adopt: Box::new(|| {}),
                })
                .unwrap();
        });
        let chunk = source.next_chunk().await.unwrap();
        assert_eq!(chunk.audio_data, vec![3.0; 4]);
    }

    #[tokio::test]
    async fn sequence_stays_monotonic_across_swap() {
        let mut source = SwitchingChunkSource::new(Constant::new(2, 1.0, 10));
        let switcher = source.switcher();
        let first = source.next_chunk().await.unwrap();
        let second = source.next_chunk().await.unwrap();
        assert!(second.sequence_number > first.sequence_number);
        switcher
            .send(SwapRequest {
                payload: Constant::new(1, 2.0, 10),
                on_adopt: Box::new(|| {}),
            })
            .unwrap();
        let third = source.next_chunk().await.unwrap();
        assert!(third.sequence_number > second.sequence_number);
    }

    #[tokio::test]
    async fn queued_rapid_swaps_last_wins() {
        let mut source = SwitchingChunkSource::new(Constant::new(2, 1.0, 10));
        let switcher = source.switcher();
        let _ = source.next_chunk().await.unwrap();
        switcher
            .send(SwapRequest {
                payload: Constant::new(2, 2.0, 10),
                on_adopt: Box::new(|| {}),
            })
            .unwrap();
        switcher
            .send(SwapRequest {
                payload: Constant::new(2, 3.0, 10),
                on_adopt: Box::new(|| {}),
            })
            .unwrap();
        let chunk = source.next_chunk().await.unwrap();
        assert_eq!(chunk.audio_data, vec![3.0; 4]);
    }

    #[tokio::test]
    async fn adopt_hook_fires_at_adoption_not_at_send() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut source = SwitchingChunkSource::new(Constant::new(2, 1.0, 10));
        let switcher = source.switcher();
        let adopted = Arc::new(AtomicBool::new(false));
        let hook_adopted = adopted.clone();
        switcher
            .send(SwapRequest {
                payload: Constant::new(2, 2.0, 10),
                on_adopt: Box::new(move || {
                    hook_adopted.store(true, Ordering::SeqCst);
                }),
            })
            .unwrap();
        assert!(!adopted.load(Ordering::SeqCst));
        let chunk = source.next_chunk().await.unwrap();
        assert_eq!(chunk.audio_data, vec![2.0; 4]);
        assert!(adopted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn silence_matches_format_and_advances_sequence() {
        let format = AudioFormat::new(2, 48000);
        let mut silence = SilenceChunkSource::new(format.clone(), 480);
        let first = silence.next_chunk().await.unwrap();
        assert_eq!(first.audio_data, vec![0.0; 960]);
        assert_eq!(first.format, format);
        let second = silence.next_chunk().await.unwrap();
        assert!(second.sequence_number > first.sequence_number);
    }
}
