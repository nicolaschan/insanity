use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
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
use cpal::{BufferSize, Device, Sample, SampleFormat, SampleRate, Stream, StreamConfig};
use insanity_core::audio_source::{AudioSource, SyncAudioSource};
use insanity_tui_adapter::AppEvent;
use tokio::sync::{broadcast, mpsc::UnboundedSender};

use crate::processor::{AUDIO_CHANNELS, AUDIO_CHUNK_SIZE, AudioChunk, MultiChannelDenoiser};
use crate::realtime_buffer::RealTimeBuffer;
use insanity_core::loudness::calculate_loudness;
use rubato_audio_source::ResampledAudioSource;

// shared config helpers

pub(crate) fn find_stereo_input(
    range: cpal::SupportedInputConfigs,
) -> Option<cpal::SupportedStreamConfigRange> {
    use itertools::Itertools;
    range
        .into_iter()
        .find_or_last(|x| x.channels() == AUDIO_CHANNELS)
}

pub(crate) fn find_stereo_output(
    range: cpal::SupportedOutputConfigs,
) -> Option<cpal::SupportedStreamConfigRange> {
    use itertools::Itertools;
    range
        .into_iter()
        .find_or_last(|x| x.channels() == AUDIO_CHANNELS)
}

pub(crate) fn get_input_config(device: &Device) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let range = device
        .supported_input_configs()
        .map_err(|e| anyhow::anyhow!(e))?;
    let cfg_range =
        find_stereo_input(range).ok_or_else(|| anyhow::anyhow!("No supported input config"))?;
    let max = cfg_range.max_sample_rate();
    let channels = cfg_range.channels();
    let sample_rate = std::cmp::min(SampleRate(48000), max);
    let buffer_size = match cfg_range.buffer_size() {
        cpal::SupportedBufferSize::Range { min: _, max: _ } => BufferSize::Default,
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    };
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
    let sample_rate = std::cmp::min(SampleRate(48000), max);
    let buffer_size = match cfg_range.buffer_size() {
        cpal::SupportedBufferSize::Range { min: _, max: _ } => BufferSize::Default,
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    };
    let cfg = StreamConfig {
        channels,
        sample_rate,
        buffer_size,
    };
    Ok((cfg_range.sample_format(), cfg))
}

fn run_input<T: Sample>(
    config: &StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<f32>,
) -> anyhow::Result<Stream> {
    let err_fn = |err| eprintln!("input stream error: {err}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for s in data.iter() {
                    let _ = sender.send(s.to_f32());
                }
            },
            err_fn,
        )
        .map_err(|e| anyhow::anyhow!("build input stream: {e}"))
}

fn setup_input_stream(
    sample_format: &SampleFormat,
    config: &StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<f32>,
) -> anyhow::Result<Stream> {
    match sample_format {
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
    }
}

// RealtimeAudioSource used for output per-peer
pub struct RealtimeAudioSource {
    chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
    sample_buffer: VecDeque<f32>,
    sample_rate: u32,
    channels: u16,
}

impl RealtimeAudioSource {
    pub fn new(
        chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        Self {
            chunk_buffer,
            sample_buffer: VecDeque::new(),
            sample_rate,
            channels,
        }
    }
}

impl AudioSource for RealtimeAudioSource {
    async fn next(&mut self) -> Option<f32> {
        self.next_sync()
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

impl SyncAudioSource for RealtimeAudioSource {
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

// Cpal receiver for single input
struct CpalStreamReceiver {
    _stream: send_safe::SendWrapperThread<Option<Stream>>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<f32>,
    sample_rate: u32,
    channels: u16,
}

impl AudioSource for CpalStreamReceiver {
    async fn next(&mut self) -> Option<f32> {
        self.receiver.recv().await
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

fn make_single_input() -> Option<CpalStreamReceiver> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let Ok((fmt, cfg)) = get_input_config(&device) else {
        log::warn!("Failed to get input config, falling back to silence");
        return None;
    };
    let cfg2 = cfg.clone();
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        match setup_input_stream(&fmt, &cfg2, &device, tx) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("Failed to build input stream, falling back to silence: {e:?}");
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
        log::warn!("Failed to start input stream, falling back to silence");
        return None;
    }
    Some(CpalStreamReceiver {
        _stream: wrapper,
        receiver: rx,
        sample_rate: cfg.sample_rate.0,
        channels: cfg.channels,
    })
}

// Single input hub

/// Hub broadcast item: wall-clock sequence number + chunk. Seq advances every
/// 10ms tick (including muted ticks, which are not sent).
pub type HubChunk = (u128, Arc<Vec<f32>>);

pub struct AudioInputHub {
    tx: broadcast::Sender<HubChunk>,
    muted: Arc<AtomicBool>,
    channels: u16,
}

impl AudioInputHub {
    pub fn new() -> Self {
        let (btx, _) = broadcast::channel(32);
        let muted = Arc::new(AtomicBool::new(false));
        let muted_clone = muted.clone();
        let btx_clone = btx.clone();

        // Synchronously probe input device to determine correct channel count for the encoder.
        // Fallback to stereo if no device.
        let initial_channels = {
            let host = cpal::default_host();
            if let Some(device) = host.default_input_device() {
                if let Ok((_fmt, cfg)) = get_input_config(&device) {
                    cfg.channels
                } else {
                    AUDIO_CHANNELS
                }
            } else {
                AUDIO_CHANNELS
            }
        };

        // spawn task that captures single input and resamples to 48000
        tokio::spawn(async move {
            let Some(receiver) = make_single_input() else {
                // no input device: send silence periodically so senders don't block forever
                let mut next_seq: u128 = 0;
                let silence: Arc<Vec<f32>> =
                    Arc::new(vec![0.0f32; AUDIO_CHUNK_SIZE * AUDIO_CHANNELS as usize]);
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    let seq = next_seq;
                    next_seq += 1;
                    if muted_clone.load(Ordering::Relaxed) {
                        continue;
                    }
                    let _ = btx_clone.send((seq, silence.clone()));
                }
            };
            let channels = receiver.channels;
            let mut resampled = ResampledAudioSource::new(receiver, 48000, AUDIO_CHUNK_SIZE);
            let mut next_seq: u128 = 0;
            loop {
                let mut chunk = Vec::with_capacity(AUDIO_CHUNK_SIZE * channels as usize);
                for _ in 0..AUDIO_CHUNK_SIZE * channels as usize {
                    if let Some(s) = resampled.next().await {
                        chunk.push(s);
                    } else {
                        return;
                    }
                }
                let seq = next_seq;
                next_seq += 1;
                if muted_clone.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
                let _ = btx_clone.send((seq, Arc::new(chunk)));
            }
        });

        Self {
            tx: btx,
            muted,
            channels: initial_channels,
        }
    }

    // test seam: single input from any AudioSource
    pub fn from_source<R>(source: R) -> Self
    where
        R: AudioSource + Send + Sync + 'static,
    {
        let (btx, _) = broadcast::channel(32);
        let muted = Arc::new(AtomicBool::new(false));
        let muted_clone = muted.clone();
        let btx_clone = btx.clone();
        let channels = source.channels();
        tokio::spawn(async move {
            let mut resampled = ResampledAudioSource::new(source, 48000, AUDIO_CHUNK_SIZE);
            let mut next_seq: u128 = 0;
            loop {
                let mut chunk = Vec::with_capacity(AUDIO_CHUNK_SIZE * channels as usize);
                for _ in 0..AUDIO_CHUNK_SIZE * channels as usize {
                    if let Some(s) = resampled.next().await {
                        chunk.push(s);
                    } else {
                        return;
                    }
                }
                let seq = next_seq;
                next_seq += 1;
                if muted_clone.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
                let _ = btx_clone.send((seq, Arc::new(chunk)));
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        Self {
            tx: btx,
            muted,
            channels,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HubChunk> {
        self.tx.subscribe()
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

// Output mixer

/// Jitter buffer target in 10ms chunks.
pub const JITTER_TARGET_CHUNKS: usize = 10;
/// Single source of truth for max volume.
pub const MAX_VOLUME: usize = 500;

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

/// Convert an incoming chunk to mixer channel space.
/// Passthrough when equal; mono->stereo duplicates, stereo->mono averages
/// `(L+R)/2`. Generic fallback round-robins source channels.
pub fn convert_to_mixer_channels(mut chunk: AudioChunk, mixer_channels: u16) -> AudioChunk {
    let src_channel_count = chunk.audio_format.channel_count;
    if src_channel_count == mixer_channels
        || src_channel_count == 0
        || mixer_channels == 0
        || chunk.audio_data.is_empty()
    {
        return chunk;
    }
    let frames = chunk.audio_data.len() / src_channel_count as usize;
    let mut out = Vec::with_capacity(frames * mixer_channels as usize);
    if src_channel_count == 1 && mixer_channels == 2 {
        for &m in chunk.audio_data.iter() {
            out.push(m);
            out.push(m);
        }
    } else if src_channel_count == 2 && mixer_channels == 1 {
        let (pairs, _) = chunk.audio_data.as_chunks::<2>();
        out.extend(pairs.iter().map(|pair| (pair[0] + pair[1]) * 0.5));
    } else {
        for f in 0..frames {
            for t in 0..mixer_channels as usize {
                out.push(
                    chunk.audio_data
                        [f * src_channel_count as usize + (t % src_channel_count as usize)],
                );
            }
        }
    }
    chunk.audio_data = out;
    chunk.audio_format.channel_count = mixer_channels;
    chunk
}

pub fn volume_multiplier(volume: usize) -> f32 {
    let vol = volume.min(MAX_VOLUME) as f32;
    let a: f32 = 0.2;
    let m = a * ((1.0 + 1.0 / a).powf(vol / 100.0) - 1.0);
    m
}

/// 1-chunk fade (~10ms stereo: 960 samples). Mono mixers fade ~20ms;
/// negligible and keeps the callback channel-agnostic.
pub const PLC_FADE_SAMPLES: usize = 960;

struct PeerState {
    chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
    audio_receiver: Mutex<ResampledAudioSource<RealtimeAudioSource>>,
    denoiser: Mutex<MultiChannelDenoiser<'static>>,
    volume: Arc<AtomicUsize>,
    enable_denoise: Arc<AtomicBool>,
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

fn build_output_stream(
    sample_format: &SampleFormat,
    config: &StreamConfig,
    device: &Device,
    state: Arc<Mutex<MixerState>>,
    master: Arc<AtomicUsize>,
    metrics: Arc<MixerMetrics>,
) -> anyhow::Result<Stream> {
    let err_fn = |err| eprintln!("output stream error: {err}");
    match sample_format {
        SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    fill_buffer_inner(&state, &master, &metrics, data);
                },
                err_fn,
            )
            .map_err(|e| anyhow::anyhow!("build f32 output stream: {e}")),
        SampleFormat::I16 => device
            .build_output_stream(
                config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    fill_buffer_inner(&state, &master, &metrics, data);
                },
                err_fn,
            )
            .map_err(|e| anyhow::anyhow!("build i16 output stream: {e}")),
        SampleFormat::U16 => device
            .build_output_stream(
                config,
                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                    fill_buffer_inner(&state, &master, &metrics, data);
                },
                err_fn,
            )
            .map_err(|e| anyhow::anyhow!("build u16 output stream: {e}")),
    }
}

impl AudioMixer {
    pub fn new_no_device() -> Self {
        Self::new_no_device_with_format(48000, AUDIO_CHANNELS)
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
            sample_rate: SampleRate(sample_rate),
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
                    let cfg2 = cfg.clone();
                    let state2 = state_clone.clone();
                    let master2 = master_clone.clone();
                    let metrics2 = metrics_clone.clone();
                    let mut wrapper = send_safe::SendWrapperThread::new(move || {
                        match build_output_stream(&fmt, &cfg2, &device, state2, master2, metrics2) {
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
                        (SampleRate(48000), AUDIO_CHANNELS, None)
                    } else {
                        (sr, ch, Some(wrapper))
                    }
                }
                Err(e) => {
                    log::warn!("Failed to get output config: {e}, falling back to dummy");
                    (SampleRate(48000), AUDIO_CHANNELS, None)
                }
            }
        } else {
            (SampleRate(48000), AUDIO_CHANNELS, None)
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
        enable_denoise: Arc<AtomicBool>,
        app_event_sender: Option<UnboundedSender<AppEvent>>,
    ) {
        let mut guard = lock(&self.state, "mixer state");
        if let Some(peer) = guard.peers.get_mut(&id) {
            // Reconnect
            lock(&peer.chunk_buffer, "chunk_buffer").clear();
            let mixer_channels = self.channels;
            let audio_receiver =
                RealtimeAudioSource::new(peer.chunk_buffer.clone(), 48000, mixer_channels);
            peer.audio_receiver = Mutex::new(ResampledAudioSource::new(
                audio_receiver,
                self.sample_rate.0,
                AUDIO_CHUNK_SIZE,
            ));
            peer.denoiser = Mutex::new(MultiChannelDenoiser::new());
            peer.last_sample.store(0.0f32.to_bits(), Ordering::Relaxed);
            peer.fade_start = 0.0;
            peer.fade_pos = 0;
            peer.volume = volume;
            peer.enable_denoise = enable_denoise;
            peer.app_event_sender = app_event_sender;
            return;
        }
        let chunk_buffer = Arc::new(Mutex::new(RealTimeBuffer::new(self.jitter_chunks)));
        let mixer_channels = self.channels;
        let audio_receiver = RealtimeAudioSource::new(chunk_buffer.clone(), 48000, mixer_channels);
        let audio_receiver =
            ResampledAudioSource::new(audio_receiver, self.sample_rate.0, AUDIO_CHUNK_SIZE);
        let state = PeerState {
            chunk_buffer,
            audio_receiver: Mutex::new(audio_receiver),
            denoiser: Mutex::new(MultiChannelDenoiser::new()),
            volume,
            enable_denoise,
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
        // Convert sender channels to mixer channels first (passthrough when
        // equal; mono->stereo dup / stereo->mono (L+R)/2 only on mismatch),
        // so denoise/gain/buffer all operate in mixer channel space.
        chunk = convert_to_mixer_channels(chunk, self.channels);
        let mut guard = lock(&self.state, "mixer state");
        let Some(peer) = guard.peers.get_mut(&id) else {
            return;
        };
        // denoise before mixing
        if peer.enable_denoise.load(Ordering::Relaxed) {
            let mut d = lock(&peer.denoiser, "denoiser");
            chunk = d.denoise_chunk(&chunk);
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

    pub fn fill_buffer<T: Sample>(&self, data: &mut [T]) {
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
        self.sample_rate.0
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

fn render_silence<T: Sample>(data: &mut [T], metrics: &Arc<MixerMetrics>, t0: std::time::Instant) {
    data.fill(Sample::from(&0.0f32));
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

fn fill_buffer_inner<T: Sample>(
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
        *out = Sample::from(&clipped);
    }
    metrics
        .fill_nanos_total
        .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
}
