use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};

/// Lock a mutex, recovering from poisoning with an error log instead of
/// panicking. Audio must stay alive: a poisoned lock means a previous holder
/// panicked, so we reclaim the guard and keep going.
fn lock<'a, T>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log::error!("{what} mutex poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, FromSample, Sample, SampleFormat, SampleRate, SizedSample, Stream,
    StreamConfig,
};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::capture::{Capture, CaptureOutput, ChunkEncoder};
use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
use insanity_core::audio::codec::{AudioEncoder, EncodedChunk};
use insanity_core::audio::denoiser::MultiChannelDenoiser;
use insanity_core::audio::jitter::JitterBuffer;
use insanity_core::audio::sample::{SampleSource, SyncSampleSource};
use insanity_core::audio::sample_ops::convert_to_mixer_channels;
use insanity_core::audio::transform::{
    ChannelMap, ChunkTransform, Mute, MuteControl, volume_multiplier,
};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_tui_adapter::AppEvent;
use tokio::sync::{broadcast, mpsc::UnboundedSender};

use crate::codec_opus::OpusEncoder;
use crate::cpal::stream_receiver::make_single_input;
use crate::denoise::nnnoiseless::NnnoiselessDenoiser;
use crate::processor::{AUDIO_CHANNELS, AUDIO_CHUNK_SIZE, AUDIO_SAMPLE_RATE, MAX_VOLUME};
use insanity_core::loudness::calculate_loudness;
use rubato_audio_source::RubatoResampler;

const UNKNOWN_DEVICE_NAME: &str = "unknown device";

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

// RealtimeAudioSource used for output per-peer
pub struct RealtimeAudioSource {
    chunk_buffer: Arc<Mutex<JitterBuffer<AudioChunk>>>,
    sample_buffer: VecDeque<f32>,
}

impl RealtimeAudioSource {
    pub fn new(chunk_buffer: Arc<Mutex<JitterBuffer<AudioChunk>>>) -> Self {
        Self {
            chunk_buffer,
            sample_buffer: VecDeque::new(),
        }
    }
}

impl SampleSource for RealtimeAudioSource {
    async fn next(&mut self) -> Option<f32> {
        self.next_sync()
    }
}

impl SyncSampleSource for RealtimeAudioSource {
    fn next_sync(&mut self) -> Option<f32> {
        if self.sample_buffer.is_empty() {
            let mut buf = lock(&self.chunk_buffer, "chunk_buffer");
            if let Some(chunk) = buf.next_item() {
                self.sample_buffer.extend(chunk.audio_data);
            }
        }
        self.sample_buffer.pop_front()
    }
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

    pub fn from_chunk_source<R>(source: R) -> Self
    where
        R: ChunkSource + Send + 'static,
    {
        Self::spawn_chunk_source(source, UNKNOWN_DEVICE_NAME.into())
    }

    fn spawn_silent(device_name: String) -> Self {
        Self::spawn_chunk_source(SilentChunkSource::default(), device_name)
    }

    fn spawn_chunk_source<R>(mut source: R, device_name: String) -> Self
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

    pub fn is_muted(&self) -> bool {
        self.mute_control.is_muted()
    }
}

// Output mixer

/// Jitter buffer target in 10ms chunks.
pub const JITTER_TARGET_CHUNKS: usize = 10;

/// Snapshot of mixer counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MixerMetricsSnapshot {
    pub gap_detected: usize,
    pub late_dropped: usize,
    pub underrun: usize,
    pub plc_hold: usize,
    pub clip_hits: usize,
    pub fills: usize,
}

/// Live counters. All relaxed-order atomics; hot-path increments only.
/// `underrun` counts missed slots (events); `plc_hold` counts synthesized samples.
#[derive(Debug, Default)]
pub struct MixerMetrics {
    pub gap_detected: AtomicUsize,
    pub late_dropped: AtomicUsize,
    pub underrun: AtomicUsize,
    pub plc_hold: AtomicUsize,
    pub clip_hits: AtomicUsize,
    pub fills: AtomicUsize,
    pub fill_nanos_total: std::sync::atomic::AtomicU64,
}

impl MixerMetrics {
    pub fn snapshot(&self) -> MixerMetricsSnapshot {
        MixerMetricsSnapshot {
            gap_detected: self.gap_detected.load(Ordering::Relaxed),
            late_dropped: self.late_dropped.load(Ordering::Relaxed),
            underrun: self.underrun.load(Ordering::Relaxed),
            plc_hold: self.plc_hold.load(Ordering::Relaxed),
            clip_hits: self.clip_hits.load(Ordering::Relaxed),
            fills: self.fills.load(Ordering::Relaxed),
        }
    }

    pub fn fill_avg_nanos(&self) -> u64 {
        let fills = self.fills.load(Ordering::Relaxed) as u64;
        if fills == 0 {
            return 0;
        }
        self.fill_nanos_total.load(Ordering::Relaxed) / fills
    }
}

pub fn buffer_starved(
    interval_underruns: usize,
    occupancies: &[(String, usize)],
    capacity_chunks: usize,
) -> bool {
    interval_underruns > 0 && occupancies.iter().any(|(_, len)| *len >= capacity_chunks)
}

pub fn format_metrics_line(
    prev: &MixerMetricsSnapshot,
    current: &MixerMetricsSnapshot,
    fill_avg_nanos: u64,
    occupancies: &[(String, usize)],
) -> String {
    let peers: Vec<String> = occupancies
        .iter()
        .map(|(id, len)| format!("{id}:{len}"))
        .collect();
    format!(
        "audio gaps={} late={} underruns={} plc={} clips={} fills={} fill_avg_ns={} peers=[{}]",
        current.gap_detected.saturating_sub(prev.gap_detected),
        current.late_dropped.saturating_sub(prev.late_dropped),
        current.underrun.saturating_sub(prev.underrun),
        current.plc_hold.saturating_sub(prev.plc_hold),
        current.clip_hits.saturating_sub(prev.clip_hits),
        current.fills.saturating_sub(prev.fills),
        fill_avg_nanos,
        peers.join(" "),
    )
}

/// 1-chunk fade (~10ms stereo: 960 samples). Mono mixers fade ~20ms;
/// negligible and keeps the callback channel-agnostic.
pub const PLC_FADE_SAMPLES: usize = 960;

struct PeerState {
    chunk_buffer: Arc<Mutex<JitterBuffer<AudioChunk>>>,
    audio_receiver: Mutex<RubatoResampler<RealtimeAudioSource>>,
    nn_denoiser: Mutex<MultiChannelDenoiser<NnnoiselessDenoiser>>,
    volume: Arc<AtomicUsize>,
    denoise: Arc<Mutex<DenoiseSelection>>,
    app_event_sender: Option<UnboundedSender<AppEvent>>,
    peer_id: String,
    last_sample: AtomicU32,
    /// Fade-to-zero PLC state: start level of current gap and position
    /// within the run. Zero means idle (no active concealment); otherwise
    /// samples remaining are `PLC_FADE_SAMPLES - fade_pos`.
    fade_start: f32,
    fade_pos: usize,
}

struct MixerState {
    peers: HashMap<uuid::Uuid, PeerState>,
}

pub struct AudioMixer {
    state: Arc<Mutex<MixerState>>,
    master_volume: Arc<AtomicUsize>,
    metrics: Arc<MixerMetrics>,
    _stream: Option<send_safe::SendWrapperThread<Option<Stream>>>,
    sample_rate: SampleRate,
    channels: u16,
    jitter_chunks: usize,
}

struct OutputTarget {
    state: Arc<Mutex<MixerState>>,
    master: Arc<AtomicUsize>,
    metrics: Arc<MixerMetrics>,
}

fn build_output_stream(
    sample_format: SampleFormat,
    config: StreamConfig,
    device: &Device,
    target: OutputTarget,
) -> anyhow::Result<Stream> {
    match sample_format {
        SampleFormat::I8 => run_output::<i8>(config, device, target),
        SampleFormat::I16 => run_output::<i16>(config, device, target),
        SampleFormat::I32 => run_output::<i32>(config, device, target),
        SampleFormat::I64 => run_output::<i64>(config, device, target),
        SampleFormat::U8 => run_output::<u8>(config, device, target),
        SampleFormat::U16 => run_output::<u16>(config, device, target),
        SampleFormat::U32 => run_output::<u32>(config, device, target),
        SampleFormat::U64 => run_output::<u64>(config, device, target),
        SampleFormat::F32 => run_output::<f32>(config, device, target),
        SampleFormat::F64 => run_output::<f64>(config, device, target),
        other => Err(anyhow::anyhow!(
            "unsupported output sample format {other:?}"
        )),
    }
}

fn run_output<T>(
    config: StreamConfig,
    device: &Device,
    target: OutputTarget,
) -> anyhow::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let err_fn = |err| eprintln!("output stream error: {err}");
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                fill_buffer_inner(&target.state, &target.master, &target.metrics, data);
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow::anyhow!("build output stream: {e}"))
}

impl AudioMixer {
    pub fn new_no_device() -> Self {
        Self::new_no_device_with_format(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS)
    }

    pub fn new_no_device_with_format(sample_rate: u32, channels: u16) -> Self {
        Self::new_no_device_with_format_and_capacity(sample_rate, channels, JITTER_TARGET_CHUNKS)
    }

    pub fn new_no_device_with_format_and_capacity(
        sample_rate: u32,
        channels: u16,
        jitter_chunks: usize,
    ) -> Self {
        let state = Arc::new(Mutex::new(MixerState {
            peers: HashMap::new(),
        }));
        let master_volume = Arc::new(AtomicUsize::new(100));
        let metrics = Arc::new(MixerMetrics::default());
        Self {
            state,
            master_volume,
            metrics,
            _stream: None,
            sample_rate,
            channels,
            jitter_chunks,
        }
    }

    pub fn new(_app_event_sender: Option<UnboundedSender<AppEvent>>) -> Self {
        let host = cpal::default_host();
        // try to get default output device; if none, create dummy mixer without stream
        let state = Arc::new(Mutex::new(MixerState {
            peers: HashMap::new(),
        }));
        let master_volume = Arc::new(AtomicUsize::new(100));
        let state_clone = state.clone();
        let master_clone = master_volume.clone();
        let metrics = Arc::new(MixerMetrics::default());
        let metrics_clone = metrics.clone();

        let output_device = host.default_output_device();
        let (sample_rate, channels, _stream) = if let Some(device) = output_device {
            match get_output_config(&device) {
                Ok((fmt, cfg)) => {
                    let sr = cfg.sample_rate;
                    let ch = cfg.channels;
                    let target = OutputTarget {
                        state: state_clone.clone(),
                        master: master_clone.clone(),
                        metrics: metrics_clone.clone(),
                    };
                    let mut wrapper = send_safe::SendWrapperThread::new(move || {
                        match build_output_stream(fmt, cfg, &device, target) {
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
                    if !play_ok {
                        log::warn!("Failed to start output stream, falling back to dummy");
                        (AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, None)
                    } else {
                        (sr, ch, Some(wrapper))
                    }
                }
                Err(e) => {
                    log::warn!("Failed to get output config: {e}, falling back to dummy");
                    (AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, None)
                }
            }
        } else {
            (AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, None)
        };

        Self {
            state,
            master_volume,
            metrics,
            _stream,
            sample_rate,
            channels,
            jitter_chunks: JITTER_TARGET_CHUNKS,
        }
    }

    pub fn add_peer(
        &self,
        id: uuid::Uuid,
        volume: Arc<AtomicUsize>,
        denoise: Arc<Mutex<DenoiseSelection>>,
        app_event_sender: Option<UnboundedSender<AppEvent>>,
    ) {
        let mut guard = lock(&self.state, "mixer state");
        if let Some(peer) = guard.peers.get_mut(&id) {
            // Reconnect
            lock(&peer.chunk_buffer, "chunk_buffer").clear();
            let mixer_channels = self.channels;
            let audio_receiver = RealtimeAudioSource::new(peer.chunk_buffer.clone());
            peer.audio_receiver = Mutex::new(RubatoResampler::new(
                audio_receiver,
                AudioFormat::new(mixer_channels, AUDIO_SAMPLE_RATE),
                self.sample_rate,
                AUDIO_CHUNK_SIZE,
            ));
            peer.nn_denoiser = Mutex::new(MultiChannelDenoiser::default());
            peer.last_sample.store(0.0f32.to_bits(), Ordering::Relaxed);
            peer.fade_start = 0.0;
            peer.fade_pos = 0;
            peer.volume = volume;
            peer.denoise = denoise;
            peer.app_event_sender = app_event_sender;
            return;
        }
        let chunk_buffer = Arc::new(Mutex::new(JitterBuffer::new(self.jitter_chunks)));
        let mixer_channels = self.channels;
        let audio_receiver = RealtimeAudioSource::new(chunk_buffer.clone());
        let audio_receiver = RubatoResampler::new(
            audio_receiver,
            AudioFormat::new(mixer_channels, AUDIO_SAMPLE_RATE),
            self.sample_rate,
            AUDIO_CHUNK_SIZE,
        );
        let state = PeerState {
            chunk_buffer,
            audio_receiver: Mutex::new(audio_receiver),
            nn_denoiser: Mutex::new(MultiChannelDenoiser::default()),
            volume,
            denoise,
            app_event_sender,
            peer_id: id.to_string(),
            last_sample: AtomicU32::new(0.0f32.to_bits()),
            fade_start: 0.0,
            fade_pos: 0,
        };
        guard.peers.insert(id, state);
    }

    pub fn remove_peer(&self, id: &uuid::Uuid) {
        lock(&self.state, "mixer state").peers.remove(id);
    }

    pub fn handle_incoming(&self, id: uuid::Uuid, mut chunk: AudioChunk) {
        chunk = convert_to_mixer_channels(chunk, self.channels);
        let mut guard = lock(&self.state, "mixer state");
        let Some(peer) = guard.peers.get_mut(&id) else {
            return;
        };
        // denoise before mixing
        match *peer.denoise.lock().unwrap() {
            DenoiseSelection::None => {}
            DenoiseSelection::Nnnoiseless => {
                let mut d = lock(&peer.nn_denoiser, "nn_denoiser");
                chunk = d.denoise_chunk(&chunk);
            }
        }

        let vol = peer.volume.load(Ordering::Relaxed);
        if vol != 100 {
            let m = volume_multiplier(vol);
            for s in chunk.audio_data.iter_mut() {
                *s *= m;
            }
        }
        if let Some(sender) = &peer.app_event_sender {
            let loudness = calculate_loudness(&chunk.audio_data);
            let _ = sender.send(AppEvent::Loudness(peer.peer_id.clone(), loudness));
        }
        let seq = chunk.sequence_number;
        let mut buf = lock(&peer.chunk_buffer, "chunk_buffer");

        let virgin = buf.is_empty() && buf.head() == 0 && buf.prev() == 0;
        if seq < buf.head() {
            self.metrics.late_dropped.fetch_add(1, Ordering::Relaxed);
        } else if virgin {
            if seq != 0 {
                self.metrics.gap_detected.fetch_add(1, Ordering::Relaxed);
            }
        } else if seq > buf.prev() && seq != buf.prev() + 1 {
            self.metrics.gap_detected.fetch_add(1, Ordering::Relaxed);
        }
        buf.set(seq, chunk);
    }

    pub fn set_master_volume(&self, vol: usize) {
        self.master_volume
            .store(vol.min(MAX_VOLUME), Ordering::Relaxed);
    }

    pub fn master_volume(&self) -> usize {
        self.master_volume.load(Ordering::Relaxed)
    }

    pub fn fill_buffer<T: Sample + FromSample<f32>>(&self, data: &mut [T]) {
        fill_buffer_inner(&self.state, &self.master_volume, &self.metrics, data);
    }

    pub fn metrics_snapshot(&self) -> MixerMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn fill_avg_nanos(&self) -> u64 {
        self.metrics.fill_avg_nanos()
    }

    /// Current queued chunks for a peer (jitter occupancy). `None` if unknown.
    pub fn peer_occupancy(&self, id: &uuid::Uuid) -> Option<usize> {
        let guard = lock(&self.state, "mixer state");
        guard
            .peers
            .get(id)
            .map(|p| lock(&p.chunk_buffer, "chunk_buffer").len())
    }

    pub fn peer_occupancies(&self) -> Vec<(String, usize)> {
        let guard = lock(&self.state, "mixer state");
        guard
            .peers
            .values()
            .map(|p| {
                (
                    p.peer_id.clone(),
                    lock(&p.chunk_buffer, "chunk_buffer").len(),
                )
            })
            .collect()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

fn render_silence<T: Sample>(data: &mut [T], metrics: &Arc<MixerMetrics>, t0: std::time::Instant) {
    data.fill(T::EQUILIBRIUM);
    metrics
        .fill_nanos_total
        .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// Drain one callback worth of samples per peer.
fn drain_peer_samples(
    state: &Arc<Mutex<MixerState>>,
    len: usize,
    metrics: &Arc<MixerMetrics>,
) -> Vec<Vec<f32>> {
    // One recv lock per peer.
    let mut guard = lock(state, "mixer state");
    let mut peer_temps: Vec<Vec<f32>> = Vec::new();
    for peer in guard.peers.values_mut() {
        let mut recv = lock(&peer.audio_receiver, "audio_receiver");
        let mut tmp = Vec::with_capacity(len);
        for _ in 0..len {
            // While a concealment run is active, emit fade without pulling
            // so one missing slot maps to a full PLC_FADE_SAMPLES run.
            if peer.fade_pos != 0 {
                metrics.plc_hold.fetch_add(1, Ordering::Relaxed);
                let t = peer.fade_pos as f32 / PLC_FADE_SAMPLES as f32;
                let s = if peer.fade_pos < PLC_FADE_SAMPLES {
                    peer.fade_start * (1.0 - t)
                } else {
                    0.0
                };
                peer.fade_pos += 1;
                if peer.fade_pos >= PLC_FADE_SAMPLES {
                    peer.fade_pos = 0;
                    peer.last_sample.store(0.0f32.to_bits(), Ordering::Relaxed);
                }
                tmp.push(s);
                continue;
            }
            match recv.next_sync() {
                Some(v) => {
                    peer.last_sample.store(v.to_bits(), Ordering::Relaxed);
                    peer.fade_start = v;
                    tmp.push(v);
                }
                None => {
                    // One-chunk fade-to-zero PLC. This pull
                    // consumed one missing slot; the remaining run is
                    // emitted without pulling to preserve timing.
                    metrics.underrun.fetch_add(1, Ordering::Relaxed);
                    metrics.plc_hold.fetch_add(1, Ordering::Relaxed);
                    peer.fade_start = f32::from_bits(peer.last_sample.load(Ordering::Relaxed));
                    peer.fade_pos = 1;
                    tmp.push(peer.fade_start);
                }
            }
        }
        peer_temps.push(tmp);
    }
    peer_temps
}

fn fill_buffer_inner<T: Sample + FromSample<f32>>(
    state: &Arc<Mutex<MixerState>>,
    master_volume: &Arc<AtomicUsize>,
    metrics: &Arc<MixerMetrics>,
    data: &mut [T],
) {
    let t0 = std::time::Instant::now();
    metrics.fills.fetch_add(1, Ordering::Relaxed);
    let master_vol = master_volume.load(Ordering::Relaxed);
    let master_mult = volume_multiplier(master_vol);
    let use_master = master_vol != 100;

    if lock(state, "mixer state").peers.is_empty() {
        render_silence(data, metrics, t0);
        return;
    }

    let peer_temps = drain_peer_samples(state, data.len(), metrics);
    if peer_temps.is_empty() {
        render_silence(data, metrics, t0);
        return;
    }
    for (i, out) in data.iter_mut().enumerate() {
        let mut mixed: f32 = peer_temps.iter().map(|tmp| tmp[i]).sum();
        if use_master {
            mixed *= master_mult;
        }
        let clipped = mixed.clamp(-1.0, 1.0);
        if clipped != mixed {
            metrics.clip_hits.fetch_add(1, Ordering::Relaxed);
        }
        *out = T::from_sample(clipped);
    }
    metrics
        .fill_nanos_total
        .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
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
