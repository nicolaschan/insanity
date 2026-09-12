use std::sync::Arc;

use cpal::Device;
use cpal::traits::{DeviceTrait, HostTrait};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::capture::{Capture, CaptureOutput, ChunkEncoder};
use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
use insanity_core::audio::codec::{AudioEncoder, EncodedChunk};
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
use insanity_core::audio::sample::SampleSource;
use insanity_core::audio::transform::{ChannelMap, ChunkTransform, Mute, MuteControl};
use tokio::sync::broadcast;

use super::codec::OpusEncoder;
use super::cpal_stream_receiver::make_single_input;
use rubato_audio_source::RubatoResampler;

// Single input hub

/// Yields a silent stereo chunk every chunk period when no input device exists.
#[derive(Clone)]
struct SilentChunkSource {
    next_sequence: u128,
    audio_config: AudioPipelineConfig,
}

impl ChunkSource for SilentChunkSource {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        tokio::time::sleep(self.audio_config.chunk_period()).await;
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(
            sequence_number,
            AudioFormat::new(
                self.audio_config.channels(),
                self.audio_config.sample_rate(),
            ),
            vec![0.0; self.audio_config.block_samples()],
        ))
    }
}

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

/// Broadcasts Opus-encoded 10ms chunks. Sequence numbers advance on every
/// chunk, including muted ones, which are not sent.
pub struct AudioInputHub {
    tx: broadcast::Sender<EncodedChunk>,
    mute_control: Arc<MuteControl>,
    device_name: String,
}

impl AudioInputHub {
    pub fn new(audio_config: AudioPipelineConfig) -> Self {
        match cpal::default_host().default_input_device() {
            Some(device) => Self::from_device(device, audio_config),
            None => {
                log::warn!("No input device, falling back to silence");
                Self::spawn_silent(UNKNOWN_DEVICE_NAME.into(), audio_config)
            }
        }
    }

    pub fn from_device(device: Device, audio_config: AudioPipelineConfig) -> Self {
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or(UNKNOWN_DEVICE_NAME.into());
        match make_single_input(device, audio_config) {
            Ok((receiver, format)) => {
                let resampled = RubatoResampler::new(
                    receiver,
                    format.clone(),
                    audio_config.sample_rate(),
                    audio_config.frames(),
                );
                let (mute, mute_control) = Mute::shared(false);
                let transform = mute.chain(ChannelMap::capped(audio_config.channels()));
                let capture = Capture::new(
                    resampled,
                    AudioFormat::new(format.channel_count, audio_config.sample_rate()),
                    audio_config.frames(),
                    transform,
                    |format: &AudioFormat| {
                        OpusEncoder::new(format.sample_rate, format.channel_count)
                    },
                );
                Self::spawn_capture(capture, device_name, mute_control, audio_config)
            }
            Err(e) => {
                log::warn!("{e}");
                Self::spawn_silent(device_name, audio_config)
            }
        }
    }

    pub fn from_chunk_source<R>(source: R, audio_config: AudioPipelineConfig) -> Self
    where
        R: ChunkSource + Send + 'static,
    {
        Self::spawn_chunk_source(source, UNKNOWN_DEVICE_NAME.into(), audio_config)
    }

    fn spawn_silent(device_name: String, audio_config: AudioPipelineConfig) -> Self {
        Self::spawn_chunk_source(
            SilentChunkSource {
                next_sequence: 0,
                audio_config,
            },
            device_name,
            audio_config,
        )
    }

    pub(crate) fn spawn_chunk_source<R>(
        mut source: R,
        device_name: String,
        audio_config: AudioPipelineConfig,
    ) -> Self
    where
        R: ChunkSource + Send + 'static,
    {
        let (mute, mute_control) = Mute::shared(false);
        let mut transform = mute.chain(ChannelMap::capped(audio_config.channels()));
        let mut encoder = ChunkEncoder::new(Self::rebuild_opus, audio_config.frames());
        let (hub, tx) = Self::with_channel(device_name, mute_control);
        tokio::spawn(async move {
            let mut pacer = Pacer::new(audio_config.chunk_period());
            while let Some(chunk) = source.next_chunk().await {
                pacer.pace().await;
                let Some(chunk) = transform.transform(chunk) else {
                    continue;
                };
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

    fn spawn_capture<R, T, E, F>(
        mut capture: Capture<R, T, E, F>,
        device_name: String,
        mute_control: Arc<MuteControl>,
        audio_config: AudioPipelineConfig,
    ) -> Self
    where
        R: SampleSource + Send + 'static,
        T: ChunkTransform + Send + 'static,
        E: AudioEncoder + Send + 'static,
        F: FnMut(&AudioFormat) -> Option<E> + Send + 'static,
    {
        let (hub, tx) = Self::with_channel(device_name, mute_control);
        tokio::spawn(async move {
            let mut pacer = Pacer::new(audio_config.chunk_period());
            loop {
                let output = capture.next_output().await;
                if matches!(output, CaptureOutput::EndOfStream) {
                    break;
                }
                pacer.pace().await;
                if let CaptureOutput::Encoded(frame) = output {
                    let _ = tx.send(frame);
                }
            }
        });
        hub
    }

    fn with_channel(
        device_name: String,
        mute_control: Arc<MuteControl>,
    ) -> (Self, broadcast::Sender<EncodedChunk>) {
        let (tx, _) = broadcast::channel(32);
        let hub = Self {
            tx: tx.clone(),
            mute_control,
            device_name,
        };
        (hub, tx)
    }

    pub fn name(&self) -> &str {
        &self.device_name
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EncodedChunk> {
        self.tx.subscribe()
    }

    pub fn set_muted(&self, muted: bool) {
        self.mute_control.set(muted);
    }
}
