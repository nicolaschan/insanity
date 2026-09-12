use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

/// Lock a mutex, recovering from poisoning with an error log instead of
/// panicking. Audio must stay alive: a poisoned lock means a previous holder
/// panicked, so we reclaim the guard and keep going.
pub(crate) fn lock<'a, T>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log::error!("{what} mutex poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::capture::{Capture, CaptureOutput, ChunkEncoder};
use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
use insanity_core::audio::codec::{AudioEncoder, EncodedChunk};
use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
use insanity_core::audio::mixer::{
    DEFAULT_JITTER_CHUNKS, DEFAULT_OUT_FRAMES, Mixer, MixerInput, MixerMetrics,
};
use insanity_core::audio::sample::{SampleSource, SyncSampleSource};
use insanity_core::audio::transform::{
    ChannelMap, ChunkTransform, Denoise, DenoiseControl, Gain, GainControl, Link, MetricsReader,
    MetricsState, Mute, MuteControl,
};
use insanity_core::user_input_event::DenoiseSelection;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::codec_opus::{OpusDecoder, OpusEncoder};
use crate::cpal::stream_receiver::make_single_input;
use crate::denoise::nnnoiseless::NnnoiselessDenoiser;
use crate::processor::{AUDIO_CHANNELS, AUDIO_CHUNK_SIZE, AUDIO_SAMPLE_RATE, MAX_VOLUME};
use rubato_audio_source::{RubatoResampler, StreamResampler};

pub const AUDIO_CALLBACK_FRAMES: u32 = 480;

fn callback_buffer_size(supported: &cpal::SupportedBufferSize) -> BufferSize {
    match supported {
        cpal::SupportedBufferSize::Range { min, max } => {
            let clamped = AUDIO_CALLBACK_FRAMES.clamp(*min, *max);
            log::debug!("requesting fixed stream buffer of {clamped} frames");
            BufferSize::Fixed(clamped)
        }
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    }
}

// shared config helpers

pub(crate) fn sample_format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(100),
        SampleFormat::F64 => Some(90),
        SampleFormat::I32 => Some(80),
        SampleFormat::U32 => Some(79),
        SampleFormat::I16 => Some(60),
        SampleFormat::U16 => Some(59),
        SampleFormat::I8 => Some(50),
        SampleFormat::U8 => Some(49),
        _ => None,
    }
}

fn best_stereo_config(
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Option<cpal::SupportedStreamConfigRange> {
    let mut ranges: Vec<cpal::SupportedStreamConfigRange> = ranges.collect();
    let best = ranges
        .iter()
        .filter(|r| r.channels() == AUDIO_CHANNELS)
        .filter_map(|r| sample_format_rank(r.sample_format()).map(|rank| (rank, r)))
        .reduce(|best, candidate| {
            if candidate.0 > best.0 {
                candidate
            } else {
                best
            }
        })
        .map(|(_, r)| *r);
    best.or_else(|| ranges.pop())
}

pub(crate) fn find_stereo_input(
    range: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Option<cpal::SupportedStreamConfigRange> {
    best_stereo_config(range)
}

pub(crate) fn find_stereo_output(
    range: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Option<cpal::SupportedStreamConfigRange> {
    best_stereo_config(range)
}

pub(crate) fn get_input_config(device: &Device) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let range = device
        .supported_input_configs()
        .map_err(|e| anyhow::anyhow!(e))?;
    let cfg_range =
        find_stereo_input(range).ok_or_else(|| anyhow::anyhow!("No supported input config"))?;
    let max = cfg_range.max_sample_rate();
    let channels = cfg_range.channels();
    let sample_rate = AUDIO_SAMPLE_RATE.min(max);
    let buffer_size = callback_buffer_size(cfg_range.buffer_size());
    let cfg = StreamConfig {
        channels,
        sample_rate,
        buffer_size,
    };
    Ok((cfg_range.sample_format(), cfg))
}

pub(crate) fn get_output_config(device: &Device) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let range = device
        .supported_output_configs()
        .map_err(|e| anyhow::anyhow!(e))?;
    let cfg_range =
        find_stereo_output(range).ok_or_else(|| anyhow::anyhow!("No supported output config"))?;
    let max = cfg_range.max_sample_rate();
    let channels = cfg_range.channels();
    let sample_rate = AUDIO_SAMPLE_RATE.min(max);
    let buffer_size = callback_buffer_size(cfg_range.buffer_size());
    let cfg = StreamConfig {
        channels,
        sample_rate,
        buffer_size,
    };
    Ok((cfg_range.sample_format(), cfg))
}

// Single input hub

/// Yields a silent stereo chunk every 10ms when no input device exists.
#[derive(Default)]
struct SilentChunkSource {
    next_sequence: u128,
}

impl ChunkSource for SilentChunkSource {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(
            sequence_number,
            AudioFormat::new(AUDIO_CHANNELS, AUDIO_SAMPLE_RATE),
            vec![0.0; AUDIO_CHUNK_SIZE * AUDIO_CHANNELS as usize],
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
        tokio::time::Duration::from_millis(
            (AUDIO_CHUNK_SIZE as u64 * 1000) / u64::from(AUDIO_SAMPLE_RATE),
        )
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
                let resampled = RubatoResampler::new(
                    receiver,
                    format.clone(),
                    AUDIO_SAMPLE_RATE,
                    AUDIO_CHUNK_SIZE,
                );
                let (mute, mute_control) = Mute::shared(false);
                let transform = mute.chain(ChannelMap::capped(AUDIO_CHANNELS));
                let capture = Capture::new(
                    resampled,
                    AudioFormat::new(format.channel_count, AUDIO_SAMPLE_RATE),
                    AUDIO_CHUNK_SIZE,
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

    fn spawn_silent(device_name: String) -> Self {
        Self::spawn_chunk_source(SilentChunkSource::default(), device_name)
    }

    pub(crate) fn spawn_chunk_source<R>(mut source: R, device_name: String) -> Self
    where
        R: ChunkSource + Send + 'static,
    {
        let (mute, mute_control) = Mute::shared(false);
        let mut transform = mute.chain(ChannelMap::capped(AUDIO_CHANNELS));
        let mut encoder = ChunkEncoder::new(Self::rebuild_opus, AUDIO_CHUNK_SIZE);
        let (hub, tx) = Self::with_channel(device_name, mute_control);
        tokio::spawn(async move {
            let mut pacer = Pacer::new(Pacer::chunk_period());
            while let Some(chunk) = source.next_chunk().await {
                let Some(chunk) = transform.transform(chunk) else {
                    continue;
                };
                let Some(frame) = encoder.encode_chunk(chunk) else {
                    continue;
                };
                pacer.pace().await;
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
                match capture.next_output().await {
                    CaptureOutput::EndOfStream => break,
                    CaptureOutput::Skipped => {}
                    CaptureOutput::Encoded(frame) => {
                        pacer.pace().await;
                        let _ = tx.send(frame);
                    }
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

// Output mixer

pub struct FillStats {
    total_nanos: AtomicU64,
    fills: AtomicUsize,
}

impl FillStats {
    pub fn new() -> Self {
        FillStats {
            total_nanos: AtomicU64::new(0),
            fills: AtomicUsize::new(0),
        }
    }

    pub fn record(&self, elapsed: std::time::Duration) {
        self.fills.fetch_add(1, Ordering::Relaxed);
        self.total_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn avg_nanos(&self) -> u64 {
        let fills = self.fills.load(Ordering::Relaxed) as u64;
        if fills == 0 {
            return 0;
        }
        self.total_nanos.load(Ordering::Relaxed) / fills
    }
}

impl Default for FillStats {
    fn default() -> Self {
        Self::new()
    }
}

pub fn format_metrics_line(
    prev: &MixerMetrics,
    current: &MixerMetrics,
    fill_avg_nanos: u64,
    peer_count: usize,
) -> String {
    format!(
        "audio gaps={} late={} underruns={} plc={} clips={} fills={} fill_avg_ns={} peers={}",
        current.gap_detected.saturating_sub(prev.gap_detected),
        current.late_dropped.saturating_sub(prev.late_dropped),
        current.underrun.saturating_sub(prev.underrun),
        current.plc_hold.saturating_sub(prev.plc_hold),
        current.clip_hits.saturating_sub(prev.clip_hits),
        current.fills.saturating_sub(prev.fills),
        fill_avg_nanos,
        peer_count,
    )
}

pub type PeerChain = Link<Denoise<NnnoiselessDenoiser>, Link<Gain, MetricsReader>>;
type OpusRebuild = fn(&AudioFormat) -> Option<OpusDecoder>;
pub type AppMixer = Mixer<OpusDecoder, PeerChain, StreamResampler, Gain, OpusRebuild>;

#[derive(Clone)]
pub struct PeerControls {
    pub gain: Arc<GainControl>,
    pub denoise: Arc<DenoiseControl>,
    pub loudness: Arc<MetricsState>,
}

impl PeerControls {
    pub(crate) fn new(volume: usize, denoise: DenoiseSelection) -> Self {
        let (_, gain) = Gain::shared(volume, MAX_VOLUME);
        let (_, denoise) = Denoise::<NnnoiselessDenoiser>::shared(denoise);
        let (_, loudness) = MetricsReader::shared();
        PeerControls {
            gain,
            denoise,
            loudness,
        }
    }
}

pub(crate) fn chain_from_controls(controls: &PeerControls) -> PeerChain {
    Denoise::new(controls.denoise.clone()).chain(
        Gain::new(controls.gain.clone()).chain(MetricsReader::new(controls.loudness.clone())),
    )
}

pub(crate) const NO_SLOT: u32 = u32::MAX;
const MIXER_OPS_BOUND: usize = 64;

enum MixerOp {
    Push {
        slot: u32,
        chunk: EncodedChunk,
    },
    Subscribe {
        controls: PeerControls,
        decoder: OpusRebuild,
        resampler: StreamResampler,
        reply: oneshot::Sender<u32>,
    },
    Unsubscribe(u32),
    Snapshot(oneshot::Sender<(MixerMetrics, usize)>),
}

#[derive(Clone)]
pub(crate) struct MixerClient {
    tx: mpsc::Sender<MixerOp>,
}

impl MixerClient {
    pub(crate) async fn push_frame(&self, slot: u32, chunk: EncodedChunk) {
        let _ = self.tx.send(MixerOp::Push { slot, chunk }).await;
    }

    pub(crate) async fn subscribe(
        &self,
        controls: PeerControls,
        decoder: OpusRebuild,
        resampler: StreamResampler,
    ) -> Option<u32> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MixerOp::Subscribe {
                controls,
                decoder,
                resampler,
                reply: reply_tx,
            })
            .await
            .ok()?;
        reply_rx.await.ok()
    }

    pub(crate) async fn unsubscribe(&self, slot: u32) {
        let _ = self.tx.send(MixerOp::Unsubscribe(slot)).await;
    }

    pub(crate) async fn snapshot(&self) -> Option<(MixerMetrics, usize)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send(MixerOp::Snapshot(reply_tx)).await.ok()?;
        reply_rx.await.ok()
    }
}

async fn run_mixer_owner(mixer: Arc<Mutex<AppMixer>>, mut rx: mpsc::Receiver<MixerOp>) {
    let mut batch = Vec::with_capacity(MIXER_OPS_BOUND);
    while rx.recv_many(&mut batch, MIXER_OPS_BOUND).await > 0 {
        let mut slot_replies = Vec::new();
        let mut snapshot_replies = Vec::new();
        {
            let mut guard = lock(&mixer, "mixer");
            for op in batch.drain(..) {
                match op {
                    MixerOp::Push { slot, chunk } => {
                        if let Some(input) = guard.input_mut(slot) {
                            input.push_frame(chunk);
                        }
                    }
                    MixerOp::Subscribe {
                        controls,
                        decoder,
                        resampler,
                        reply,
                    } => {
                        let slot =
                            guard.subscribe(chain_from_controls(&controls), decoder, resampler);
                        slot_replies.push((reply, slot));
                    }
                    MixerOp::Unsubscribe(slot) => guard.unsubscribe(slot),
                    MixerOp::Snapshot(reply) => {
                        snapshot_replies
                            .push((reply, (guard.metrics_snapshot(), guard.peer_count())));
                    }
                }
            }
        }
        for (reply, slot) in slot_replies {
            let _ = reply.send(slot);
        }
        for (reply, snapshot) in snapshot_replies {
            let _ = reply.send(snapshot);
        }
    }
}

pub fn rebuild_opus_decoder(format: &AudioFormat) -> Option<OpusDecoder> {
    OpusDecoder::new(format.sample_rate, format.channel_count)
}

pub fn output_resampler(out: AudioFormat, block_frames: usize) -> StreamResampler {
    StreamResampler::new(
        AudioFormat::new(out.channel_count, AUDIO_SAMPLE_RATE),
        out.sample_rate,
        block_frames,
    )
}

/// Live output handles. Besides the mixer client, timing stats, and format,
/// this owns the cpal output `Stream`: dropping it stops the audio callback,
/// so the stream must live as long as the program does.
pub(crate) struct AudioOutput {
    pub(crate) client: MixerClient,
    pub(crate) timing: Arc<FillStats>,
    pub(crate) format: AudioFormat,
    _stream: Option<send_safe::SendWrapperThread<Option<Stream>>>,
}

pub(crate) fn start_output() -> AudioOutput {
    let host = cpal::default_host();
    let output = host
        .default_output_device()
        .and_then(|device| match get_output_config(&device) {
            Ok((sample_format, config)) => Some((device, sample_format, config)),
            Err(e) => {
                log::warn!("Failed to get output config, falling back to dummy: {e}");
                None
            }
        });
    let format = output
        .as_ref()
        .map(|(_, _, config)| AudioFormat::new(config.channels, config.sample_rate))
        .unwrap_or(AudioFormat::new(AUDIO_CHANNELS, AUDIO_SAMPLE_RATE));
    let (bus, _) = Gain::shared(100, MAX_VOLUME);
    let mixer: Arc<Mutex<AppMixer>> = Arc::new(Mutex::new(Mixer::new(
        format.clone(),
        DEFAULT_JITTER_CHUNKS,
        DEFAULT_OUT_FRAMES,
        bus,
    )));
    let timing = Arc::new(FillStats::new());
    let callback_mixer = mixer.clone();
    let callback_timing = timing.clone();
    let stream = match output {
        Some((device, sample_format, config)) => {
            let mut wrapper =
                send_safe::SendWrapperThread::new(move || {
                    match build_output_stream(
                        sample_format,
                        config,
                        &device,
                        &callback_mixer,
                        &callback_timing,
                    ) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            log::warn!(
                                "Failed to build output stream, falling back to dummy: {e:?}"
                            );
                            None
                        }
                    }
                });
            let play_ok = wrapper
                .execute(|s| match s {
                    Some(stream) => stream.play().is_ok(),
                    None => false,
                })
                .unwrap_or(false);
            if play_ok {
                Some(wrapper)
            } else {
                log::warn!("Failed to start output stream, falling back to dummy");
                None
            }
        }
        None => None,
    };
    let (op_tx, op_rx) = mpsc::channel(MIXER_OPS_BOUND);
    let owner_mixer = mixer.clone();
    tokio::spawn(async move { run_mixer_owner(owner_mixer, op_rx).await });
    AudioOutput {
        client: MixerClient { tx: op_tx },
        timing,
        format,
        _stream: stream,
    }
}

fn build_output_stream(
    sample_format: SampleFormat,
    config: StreamConfig,
    device: &Device,
    mixer: &Arc<Mutex<AppMixer>>,
    timing: &Arc<FillStats>,
) -> anyhow::Result<Stream> {
    match sample_format {
        SampleFormat::I8 => run_output::<i8>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::I16 => run_output::<i16>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::I32 => run_output::<i32>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::I64 => run_output::<i64>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::U8 => run_output::<u8>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::U16 => run_output::<u16>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::U32 => run_output::<u32>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::U64 => run_output::<u64>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::F32 => run_output::<f32>(config, device, mixer.clone(), timing.clone()),
        SampleFormat::F64 => run_output::<f64>(config, device, mixer.clone(), timing.clone()),
        other => Err(anyhow::anyhow!(
            "unsupported output sample format {other:?}"
        )),
    }
}

fn run_output<T>(
    config: StreamConfig,
    device: &Device,
    mixer: Arc<Mutex<AppMixer>>,
    timing: Arc<FillStats>,
) -> anyhow::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let err_fn = |err| eprintln!("output stream error: {err}");
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let start = std::time::Instant::now();
                {
                    let mut mixer = lock(&mixer, "mixer");
                    for out in data.iter_mut() {
                        *out = T::from_sample(mixer.next_sync().unwrap_or(0.0));
                    }
                }
                timing.record(start.elapsed());
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow::anyhow!("build output stream: {e}"))
}

#[cfg(test)]
mod format_selection_tests {
    use super::{find_stereo_input, find_stereo_output, sample_format_rank};
    use cpal::SampleFormat;

    fn range(channels: u16, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            44100,
            48000,
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn fidelity_ordering_prefers_float_then_width_then_signed() {
        let ranked = [
            SampleFormat::F32,
            SampleFormat::F64,
            SampleFormat::I32,
            SampleFormat::U32,
            SampleFormat::I16,
            SampleFormat::U16,
            SampleFormat::I8,
            SampleFormat::U8,
        ];
        let scores: Vec<u8> = ranked
            .iter()
            .map(|f| sample_format_rank(*f).expect("usable"))
            .collect();
        let mut ordered = scores.clone();
        ordered.sort();
        ordered.reverse();
        assert_eq!(scores, ordered);
    }

    #[test]
    fn packed_and_dsd_formats_are_unusable() {
        for format in [
            SampleFormat::I24,
            SampleFormat::U24,
            SampleFormat::DsdU8,
            SampleFormat::DsdU16,
            SampleFormat::DsdU32,
        ] {
            assert_eq!(sample_format_rank(format), None);
        }
    }

    #[test]
    fn u8_first_list_selects_f32() {
        let configs = vec![
            range(2, SampleFormat::U8),
            range(2, SampleFormat::I16),
            range(2, SampleFormat::F32),
        ];
        let picked = find_stereo_input(configs.into_iter()).expect("config");
        assert_eq!(picked.sample_format(), SampleFormat::F32);
        assert_eq!(picked.channels(), 2);
        let configs = vec![
            range(2, SampleFormat::U8),
            range(2, SampleFormat::I16),
            range(2, SampleFormat::F32),
        ];
        let picked = find_stereo_output(configs.into_iter()).expect("config");
        assert_eq!(picked.sample_format(), SampleFormat::F32);
    }

    #[test]
    fn mono_configs_never_win_over_stereo() {
        let configs = vec![range(1, SampleFormat::F32), range(2, SampleFormat::U8)];
        let picked = find_stereo_input(configs.into_iter()).expect("config");
        assert_eq!(picked.channels(), 2);
        assert_eq!(picked.sample_format(), SampleFormat::U8);
    }

    #[test]
    fn dsd_stereo_is_skipped_for_fallback() {
        let configs = vec![range(2, SampleFormat::DsdU8), range(1, SampleFormat::F32)];
        let picked = find_stereo_input(configs.into_iter()).expect("config");
        assert_ne!(picked.sample_format(), SampleFormat::DsdU8);
    }

    #[test]
    fn empty_list_selects_nothing() {
        let picked = find_stereo_input(Vec::new().into_iter());
        assert!(picked.is_none());
        let picked = find_stereo_output(Vec::new().into_iter());
        assert!(picked.is_none());
    }

    #[test]
    fn fidelity_tie_keeps_first_range() {
        let first = cpal::SupportedStreamConfigRange::new(
            2,
            44100,
            48000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        let second = cpal::SupportedStreamConfigRange::new(
            2,
            8000,
            96000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        let picked = find_stereo_output(vec![first, second].into_iter()).expect("config");
        assert_eq!(picked.max_sample_rate(), 48000);
    }
}
