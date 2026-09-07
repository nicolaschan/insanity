use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
use tokio::sync::mpsc::{Receiver, Sender, channel};

pub struct SwitchingChunkSource<T> {
    source: Option<T>,
    swap_tx: Sender<T>,
    swap_rx: Receiver<T>,
}

impl<T: ChunkSource + Send> SwitchingChunkSource<T> {
    pub fn new(source: T) -> Self {
        let (swap_tx, swap_rx) = channel(1);
        Self {
            source: Some(source),
            swap_tx,
            swap_rx,
        }
    }

    pub fn switcher(&self) -> Sender<T> {
        self.swap_tx.clone()
    }
}

impl<T: ChunkSource + Send> ChunkSource for SwitchingChunkSource<T> {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        loop {
            let Some(source) = self.source.as_mut() else {
                self.source = self.swap_rx.recv().await;
                continue;
            };
            tokio::select! {
                biased;
                swapped = self.swap_rx.recv() => {
                    self.source = swapped;
                }
                chunk = source.next_chunk() => match chunk {
                    Some(c) => return Some(c),
                    None => self.source = None,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::SwitchingChunkSource;
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
        switcher.send(Constant::new(1, 2.0, 10)).await.unwrap();
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
            switcher.send(Constant::new(2, 3.0, 1)).await.unwrap();
        });
        let chunk = source.next_chunk().await.unwrap();
        assert_eq!(chunk.audio_data, vec![3.0; 4]);
    }
}
