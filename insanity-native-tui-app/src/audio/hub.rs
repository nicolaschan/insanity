use std::sync::Arc;

use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::ChunkSource;
use insanity_core::audio::codec::ChunkEncoder;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::transform::{ChannelMap, ChunkTransform, Mute, MuteControl};
#[cfg(feature = "encode-silence")]
use insanity_core::audio::transform::{Hysteresis, RmsDetector, SilenceGate};
use tokio::sync::broadcast;

#[cfg(feature = "encode-silence")]
use super::mixer::{QUIET_HANGOVER_CHUNKS, QUIET_RMS_THRESHOLD};

/// Broadcasts chunks
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
        #[cfg(feature = "encode-silence")]
        let mut transform = mute
            .chain(ChannelMap::capped(audio_config.channels()))
            .chain(SilenceGate::new(
                ChunkEncoder::new(rebuild, audio_config.frames()),
                Hysteresis::new(RmsDetector::new(QUIET_RMS_THRESHOLD), QUIET_HANGOVER_CHUNKS),
            ));
        #[cfg(not(feature = "encode-silence"))]
        let mut transform = mute
            .chain(ChannelMap::capped(audio_config.channels()))
            .chain(ChunkEncoder::new(rebuild, audio_config.frames()));
        let (tx, _) = broadcast::channel(32);
        let hub = Self {
            tx: tx.clone(),
            mute_control,
        };
        tokio::spawn(async move {
            while let Some(chunk) = source.next_chunk().await {
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
