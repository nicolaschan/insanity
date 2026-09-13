use futures_util::stream::{self, BoxStream, Stream, StreamExt};
use insanity_core::audio::chunk::AudioChunk;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc::{Receiver, Sender, channel};

pub struct SwitchingChunkSource<T> {
    chunks: BoxStream<'static, AudioChunk>,
    swap_tx: Sender<T>,
}

impl<T: Stream<Item = AudioChunk> + Send + 'static> SwitchingChunkSource<T> {
    pub fn new(source: T) -> Self {
        let (swap_tx, swap_rx) = channel(1);
        Self {
            chunks: switching(source, swap_rx).boxed(),
            swap_tx,
        }
    }

    pub fn switcher(&self) -> Sender<T> {
        self.swap_tx.clone()
    }
}

fn switching<T: Stream<Item = AudioChunk> + Send>(
    source: T,
    swap_rx: Receiver<T>,
) -> impl Stream<Item = AudioChunk> + Send {
    stream::unfold(
        (Some(Box::pin(source)), swap_rx),
        |(mut source, mut swap_rx)| async move {
            loop {
                let Some(current) = source.as_mut() else {
                    source = Some(Box::pin(swap_rx.recv().await?));
                    continue;
                };
                tokio::select! {
                    biased;
                    swapped = swap_rx.recv() => source = Some(Box::pin(swapped?)),
                    chunk = current.next() => match chunk {
                        Some(chunk) => return Some((chunk, (source, swap_rx))),
                        None => source = None,
                    },
                }
            }
        },
    )
}

impl<T> Stream for SwitchingChunkSource<T> {
    type Item = AudioChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AudioChunk>> {
        self.chunks.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::SwitchingChunkSource;
    use futures_util::{Stream, StreamExt, stream};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::AudioChunk;

    fn constant(
        channels: u16,
        value: f32,
        remaining: usize,
    ) -> impl Stream<Item = AudioChunk> + Send {
        let chunk = AudioChunk::new(0, AudioFormat::new(channels, 48000), vec![value; 4]);
        stream::iter(std::iter::repeat_n(chunk, remaining))
    }

    #[tokio::test]
    async fn swap_takes_effect_before_next_chunk_and_carries_format() {
        let mut source = SwitchingChunkSource::new(constant(2, 1.0, 10));
        let switcher = source.switcher();
        let first = source.next().await.unwrap();
        assert_eq!(first.audio_data, vec![1.0; 4]);
        assert_eq!(first.format.channel_count, 2);
        switcher.send(constant(1, 2.0, 10)).await.unwrap();
        let second = source.next().await.unwrap();
        assert_eq!(second.audio_data, vec![2.0; 4]);
        assert_eq!(second.format.channel_count, 1);
    }

    #[tokio::test]
    async fn exhausted_source_parks_until_swapped() {
        let mut source = SwitchingChunkSource::new(constant(2, 1.0, 1));
        let switcher = source.switcher();
        assert!(source.next().await.is_some());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            switcher.send(constant(2, 3.0, 1)).await.unwrap();
        });
        let chunk = source.next().await.unwrap();
        assert_eq!(chunk.audio_data, vec![3.0; 4]);
    }
}
