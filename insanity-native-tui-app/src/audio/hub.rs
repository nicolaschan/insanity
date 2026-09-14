use std::sync::Arc;

use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::{ChunkSource, SampleChunker};
use insanity_core::audio::codec::{ChunkEncoder, EncodedChunk};
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::sample::SampleSource;
use insanity_core::audio::transform::{ChannelMap, ChunkTransform, Mute, MuteControl};
use tokio::sync::broadcast;

use super::codec::OpusEncoder;
use rubato_audio_source::RubatoResampler;

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
pub struct AudioInputHub<OutputT = EncodedChunk> {
    tx: broadcast::Sender<OutputT>,
    mute_control: Arc<MuteControl>,
}

impl<OutputT: Clone + Send + 'static> AudioInputHub<OutputT> {
    pub fn new<T, E, F>(source: T, audio_config: AudioPipelineConfig, rebuild: F) -> Self
    where
        T: SampleSource + Send + 'static,
        E: ChunkTransform<OutputT = Option<OutputT>> + 'static,
        F: FnMut(&AudioFormat) -> Option<E> + Send + 'static,
    {
        let resampled =
            RubatoResampler::new(source, audio_config.sample_rate(), audio_config.frames());
        let transform = ChannelMap::capped(audio_config.channels());
        let source = SampleChunker::new(resampled, audio_config.frames()).transform(transform);
        Self::from_chunk_source(source, audio_config, rebuild)
    }

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

pub fn rebuild_opus_encoder(format: &AudioFormat) -> Option<OpusEncoder> {
    OpusEncoder::new(format.sample_rate, format.channel_count)
}
