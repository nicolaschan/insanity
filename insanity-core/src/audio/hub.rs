use std::sync::Arc;

use crate::audio::AudioFormat;
use crate::audio::chunk::ChunkSource;
use crate::audio::codec::ChunkEncoder;
use crate::audio::config::AudioPipelineConfig;
use crate::audio::transform::{ChannelMap, ChunkTransform, Mute, MuteControl};
use tokio::sync::broadcast;

struct Pacer {
    period: tokio::time::Duration,
    next_deadline: Option<tokio::time::Instant>,
}

impl Pacer {
    fn new(period: tokio::time::Duration) -> Self {
        Self {
            period,
            next_deadline: None,
        }
    }

    async fn pace(&mut self) {
        let now = tokio::time::Instant::now();
        let Some(deadline) = self.next_deadline else {
            self.next_deadline = Some(now + self.period);
            return;
        };
        if now < deadline {
            tokio::time::sleep_until(deadline).await;
            self.next_deadline = Some(deadline + self.period);
        } else {
            self.next_deadline = Some(now + self.period);
        }
    }
}

/// Broadcasts encoded chunks; muted chunks are sent as silence.
pub struct AudioInputHub<OutputT> {
    tx: broadcast::Sender<OutputT>,
    mute_control: Arc<MuteControl>,
}

impl<OutputT: Clone + Send + 'static> AudioInputHub<OutputT> {
    pub fn from_chunk_source<R, E, F>(
        mut source: R,
        audio_config: AudioPipelineConfig,
        rebuild: F,
    ) -> Self
    where
        R: ChunkSource + Send + 'static,
        E: ChunkTransform<OutputT = Option<OutputT>> + 'static,
        F: FnMut(&AudioFormat) -> Option<E> + Send + 'static,
    {
        let (mute, mute_control) = Mute::shared(false);
        let mut transform = mute
            .chain(ChannelMap::capped(audio_config.channels()))
            .chain(ChunkEncoder::new(rebuild, audio_config.frames()));
        let (tx, _) = broadcast::channel(32);
        let hub = Self {
            tx: tx.clone(),
            mute_control,
        };
        tokio::spawn(async move {
            let mut pacer = Pacer::new(audio_config.chunk_period());
            while let Some(chunk) = source.next_chunk().await {
                pacer.pace().await;
                let Some(frame) = transform.transform(chunk) else {
                    continue;
                };
                let _ = tx.send(frame);
            }
        });
        hub
    }

    pub fn subscribe(&self) -> broadcast::Receiver<OutputT> {
        self.tx.subscribe()
    }

    pub fn set_muted(&self, muted: bool) {
        self.mute_control.set(muted);
    }
}
