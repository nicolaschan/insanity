use std::sync::Arc;

use futures_util::{Stream, StreamExt};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::codec::{ChunkEncoder, EncodedChunk};
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::sample::AudioStream;
use insanity_core::audio::transform::{ChannelMap, ChunkTransform, Mute, MuteControl};
use tokio::sync::broadcast;

use super::codec::OpusEncoder;
use rubato_audio_source::resample;

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

/// Broadcasts Opus-encoded 10ms chunks; muted chunks are sent as silence.
pub struct AudioInputHub {
    tx: broadcast::Sender<EncodedChunk>,
    mute_control: Arc<MuteControl>,
}

impl AudioInputHub {
    pub fn new(source: AudioStream, audio_config: AudioPipelineConfig) -> Self {
        let chunks = resample(source, audio_config.sample_rate(), audio_config.frames())
            .into_chunks(audio_config.frames());
        Self::from_chunk_source(chunks, audio_config)
    }

    pub fn from_chunk_source<R>(source: R, audio_config: AudioPipelineConfig) -> Self
    where
        R: Stream<Item = AudioChunk> + Send + 'static,
    {
        let (mute, mute_control) = Mute::shared(false);
        let mut transform = mute.chain(ChannelMap::capped(audio_config.channels()));
        let mut source = Box::pin(source.map(move |chunk| transform.transform(chunk)));
        let mut encoder = ChunkEncoder::new(Self::rebuild_opus, audio_config.frames());
        let (hub, tx) = Self::with_channel(mute_control);
        tokio::spawn(async move {
            let mut pacer = Pacer::new(audio_config.chunk_period());
            while let Some(chunk) = source.next().await {
                pacer.pace().await;
                let Some(frame) = encoder.encode_chunk(chunk) else {
                    continue;
                };
                let _ = tx.send(frame);
            }
        });
        hub
    }

    fn rebuild_opus(format: &AudioFormat) -> Option<OpusEncoder> {
        OpusEncoder::new(format.sample_rate, format.channel_count)
    }

    fn with_channel(mute_control: Arc<MuteControl>) -> (Self, broadcast::Sender<EncodedChunk>) {
        let (tx, _) = broadcast::channel(32);
        let hub = Self {
            tx: tx.clone(),
            mute_control,
        };
        (hub, tx)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EncodedChunk> {
        self.tx.subscribe()
    }

    pub fn set_muted(&self, muted: bool) {
        self.mute_control.set(muted);
    }
}
