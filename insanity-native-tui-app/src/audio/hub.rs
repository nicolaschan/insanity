use std::sync::Arc;

use cpal::Device;
use cpal::traits::{DeviceTrait, HostTrait};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::capture::{Capture, CaptureOutput, ChunkEncoder};
use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
use insanity_core::audio::codec::{AudioEncoder, EncodedChunk};
use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
use insanity_core::audio::sample::SampleSource;
use insanity_core::audio::transform::{ChannelMap, ChunkTransform, Mute, MuteControl};
use tokio::sync::broadcast;

use super::codec::OpusEncoder;
use super::cpal_stream_receiver::make_single_input;
use super::params::{CHANNELS, CHUNK_PERIOD, CHUNK_SIZE, SAMPLE_RATE};
use rubato_audio_source::RubatoResampler;

// Single input hub

/// Yields a silent stereo chunk every CHUNK_PERIOD when no input device exists.
#[derive(Default)]
struct SilentChunkSource {
    next_sequence: u128,
}

impl ChunkSource for SilentChunkSource {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        tokio::time::sleep(CHUNK_PERIOD).await;
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(
            sequence_number,
            AudioFormat::new(CHANNELS, SAMPLE_RATE),
            vec![0.0; CHUNK_SIZE * CHANNELS as usize],
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

    fn chunk_period() -> tokio::time::Duration {
        CHUNK_PERIOD
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

impl Default for AudioInputHub {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioInputHub {
    pub fn new() -> Self {
        let host = cpal::default_host();
        let device = host.default_input_device().unwrap();
        Self::from_device(device)
    }

    pub fn from_device(device: Device) -> Self {
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or(UNKNOWN_DEVICE_NAME.into());
        match make_single_input(device) {
            Ok((receiver, format)) => {
                let resampled =
                    RubatoResampler::new(receiver, format.clone(), SAMPLE_RATE, CHUNK_SIZE);
                let (mute, mute_control) = Mute::shared(false);
                let transform = mute.chain(ChannelMap::capped(CHANNELS));
                let capture = Capture::new(
                    resampled,
                    AudioFormat::new(format.channel_count, SAMPLE_RATE),
                    CHUNK_SIZE,
                    transform,
                    |format: &AudioFormat| {
                        OpusEncoder::new(format.sample_rate, format.channel_count)
                    },
                );
                Self::spawn_capture(capture, device_name, mute_control)
            }
            Err(e) => {
                log::warn!("{e}");
                Self::spawn_silent(device_name)
            }
        }
    }

    pub fn from_chunk_source<R>(source: R) -> Self
    where
        R: ChunkSource + Send + 'static,
    {
        Self::spawn_chunk_source(source, UNKNOWN_DEVICE_NAME.into())
    }

    fn spawn_silent(device_name: String) -> Self {
        Self::spawn_chunk_source(SilentChunkSource::default(), device_name)
    }

    pub(crate) fn spawn_chunk_source<R>(mut source: R, device_name: String) -> Self
    where
        R: ChunkSource + Send + 'static,
    {
        let (mute, mute_control) = Mute::shared(false);
        let mut transform = mute.chain(ChannelMap::capped(CHANNELS));
        let mut encoder = ChunkEncoder::new(Self::rebuild_opus, CHUNK_SIZE);
        let (hub, tx) = Self::with_channel(device_name, mute_control);
        tokio::spawn(async move {
            let mut pacer = Pacer::new(Pacer::chunk_period());
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
    ) -> Self
    where
        R: SampleSource + Send + 'static,
        T: ChunkTransform + Send + 'static,
        E: AudioEncoder + Send + 'static,
        F: FnMut(&AudioFormat) -> Option<E> + Send + 'static,
    {
        let (hub, tx) = Self::with_channel(device_name, mute_control);
        tokio::spawn(async move {
            let mut pacer = Pacer::new(Pacer::chunk_period());
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
