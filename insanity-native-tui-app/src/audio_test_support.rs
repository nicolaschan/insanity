//! Shared e2e audio harness: virtual insanity nodes + waveform assertions.
//!
//! Each [`VirtualNode`] is one insanity program with a synthetic mic
//! ([`SineSource`] via [`AudioInputHub::from_source`]) and a virtual speaker
//! ([`AudioMixer::new_no_device`]). [`transfer_tick`] moves one 10ms chunk
//! along a directed edge through the **production** pipeline
//! (hub seq → Opus encode → bincode round-trip → Opus decode →
//! `handle_incoming`), and [`run_mesh`] drives a full topology tick by tick
//! (interleaved feed/fill, matching the realtime pattern).
//!
//! Waveform assertions are delay-tolerant: Opus introduces codec delay, so
//! [`max_normalized_xcorr`] slides the reference over ±lag before scoring.

use crate::audio::{AudioInputHub, AudioMixer};
use crate::clerver::{decode_frame_to_chunk, encode_hub_chunk};
use crate::protocol::ProtocolMessage;
use insanity_core::audio_source::{AudioSource, SyncAudioSource};
use insanity_core::loudness::calculate_loudness;
use opus::{Application, Channels, Decoder, Encoder};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize},
};
use std::time::Duration;
use tokio::sync::broadcast;

pub const PULL_TIMEOUT: Duration = Duration::from_millis(200);
pub const TRANSFER_TIMEOUT: Duration = Duration::from_millis(500);

pub fn mesh_timeout(ticks: usize, edges: usize) -> Duration {
    let per_tick = TRANSFER_TIMEOUT
        .checked_mul(edges as u32)
        .unwrap_or(Duration::from_secs(30))
        .saturating_add(Duration::from_millis(100));
    per_tick
        .checked_mul(ticks as u32)
        .unwrap_or(Duration::from_secs(120))
        .saturating_add(Duration::from_secs(5))
}

/// Deterministic tonal mic source. Phase-continuous across chunks.
pub struct SineSource {
    phase: f32,
    sr: u32,
    ch: u16,
    freq: f32,
    amp: f32,
}

impl SineSource {
    pub fn new_amp(sr: u32, ch: u16, freq: f32, amp: f32) -> Self {
        Self {
            phase: 0.0,
            sr,
            ch,
            freq,
            amp,
        }
    }

    fn step(&mut self) -> f32 {
        let v = (self.phase * 2.0 * std::f32::consts::PI).sin() * self.amp;
        self.phase = (self.phase + self.freq / self.sr as f32) % 1.0;
        v
    }
}

impl AudioSource for SineSource {
    async fn next(&mut self) -> Option<f32> {
        Some(self.step())
    }

    fn sample_rate(&self) -> u32 {
        self.sr
    }

    fn channels(&self) -> u16 {
        self.ch
    }
}

impl SyncAudioSource for SineSource {
    fn next_sync(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

/// One virtual insanity program: synthetic mic + hub, mixer + speaker tap.
pub struct VirtualNode {
    pub hub: Arc<AudioInputHub>,
    pub mixer: AudioMixer,
    pub peer_ids: HashMap<String, uuid::Uuid>,
    /// One Opus encoder per outbound edge (mirrors production, where each
    /// peer connection runs its own `run_audio_sender` + `Encoder`).
    encoders: HashMap<String, Encoder>,
    decoders: HashMap<String, Decoder>,
    /// One hub broadcast receiver per outbound edge (mirrors production, where
    /// each peer connection holds its own `hub.subscribe()`).
    hub_taps: HashMap<String, broadcast::Receiver<crate::audio::HubChunk>>,
    /// Mic chunks consumed so far (post-resample 48kHz), for reference.
    /// Deduplicated by seq so fan-out edges don't double-count.
    pub mic_history: Vec<f32>,
    mic_last_seq: Option<u128>,
    /// Speaker samples rendered so far, for assertions.
    pub speaker_history: Vec<f32>,
}

impl VirtualNode {
    pub fn new(_name: &str, freq: f32) -> Self {
        Self::with_amp(_name, freq, 0.5)
    }

    pub fn with_amp(_name: &str, freq: f32, amp: f32) -> Self {
        Self::with_source(_name, SineSource::new_amp(48000, 2, freq, amp))
    }

    /// Register an inbound peer (creates jitter/denoise/volume state).
    pub fn with_source<S>(_name: &str, source: S) -> Self
    where
        S: AudioSource + Send + Sync + 'static,
    {
        // Source must be stereo; any sample rate is OK (the hub resamples
        // to 48kHz, and the test codec is fixed Stereo/48000).
        debug_assert_eq!(source.channels(), 2, "harness codec is stereo-only");
        let hub = Arc::new(AudioInputHub::from_source(source));
        Self {
            hub,
            hub_taps: HashMap::new(),
            mixer: AudioMixer::new_no_device(),
            peer_ids: HashMap::new(),
            encoders: HashMap::new(),
            decoders: HashMap::new(),
            mic_history: Vec::new(),
            mic_last_seq: None,
            speaker_history: Vec::new(),
        }
    }

    pub fn add_inbound(&mut self, peer_name: &str) {
        self.add_inbound_denoise(peer_name, false);
    }

    /// Register an inbound peer with explicit denoise flag.
    pub fn add_inbound_denoise(&mut self, peer_name: &str, denoise: bool) {
        let id = uuid::Uuid::new_v4();
        self.peer_ids.insert(peer_name.to_string(), id);
        self.mixer.add_peer(
            id,
            Arc::new(AtomicUsize::new(100)),
            Arc::new(AtomicBool::new(denoise)),
            None,
        );
        self.decoders.insert(
            peer_name.to_string(),
            Decoder::new(48000, Channels::Stereo).expect("test decoder"),
        );
    }

    /// Register an outbound edge (dedicated broadcast tap, like production).
    pub fn add_outbound(&mut self, peer_name: &str) {
        self.hub_taps
            .insert(peer_name.to_string(), self.hub.subscribe());
        self.encoders.insert(
            peer_name.to_string(),
            Encoder::new(48000, Channels::Stereo, Application::Audio).expect("test encoder"),
        );
    }

    pub fn set_muted(&self, muted: bool) {
        self.hub.set_muted(muted);
    }

    pub fn metrics_snapshot(&self) -> crate::audio::MixerMetricsSnapshot {
        self.mixer.metrics_snapshot()
    }

    pub async fn pull_frame(&mut self, peer_name: &str) -> Option<Vec<u8>> {
        let tap = self.hub_taps.get_mut(peer_name)?;
        let (seq, chunk) = match tokio::time::timeout(PULL_TIMEOUT, tap.recv()).await {
            Ok(Ok(c)) => c,
            Ok(Err(_)) | Err(_) => return None,
        };
        let encoder = match self.encoders.get_mut(peer_name) {
            Some(e) => e,
            None => return None,
        };
        let frame = encode_hub_chunk(encoder, seq, &chunk[..])?;
        let mut buf = Vec::new();
        ProtocolMessage::AudioFrame(frame)
            .write_to_stream(&mut buf)
            .await
            .ok()?;
        if self.mic_last_seq != Some(seq) {
            self.mic_last_seq = Some(seq);
            self.mic_history.extend(chunk.iter());
        }
        Some(buf)
    }

    pub async fn push_frame(&mut self, peer_name: &str, bytes: &[u8]) -> bool {
        let Ok(ProtocolMessage::AudioFrame(frame)) =
            ProtocolMessage::read_from_stream(&mut &bytes[..]).await
        else {
            return false;
        };
        let decoder = match self.decoders.get_mut(peer_name) {
            Some(d) => d,
            None => return false,
        };
        let out = match decode_frame_to_chunk(
            decoder,
            &frame,
            self.mixer.channels(),
            self.mixer.sample_rate(),
        ) {
            Some(o) => o,
            None => return false,
        };
        let id = match self.peer_ids.get(peer_name) {
            Some(id) => *id,
            None => return false,
        };
        self.mixer.handle_incoming(id, out);
        true
    }
}

/// Move one 10ms chunk along `tx_name -> rx_name` through the production
/// encode → bincode round-trip → decode path. Returns `false` when the sender
/// had nothing to send this tick (muted hub or lagged). Borrows are scoped so
/// a shared sender can fan out to several receivers per tick (one hub chunk
/// pulled per edge, matching one broadcast receiver per peer).
pub async fn transfer_tick(
    nodes: &mut HashMap<String, VirtualNode>,
    tx_name: &str,
    rx_name: &str,
) -> bool {
    let frame_bytes = {
        let tx = nodes.get_mut(tx_name).expect("test node");
        match tx.pull_frame(rx_name).await {
            Some(b) => b,
            None => return false,
        }
    };
    let rx = nodes.get_mut(rx_name).expect("test node");
    rx.push_frame(tx_name, &frame_bytes).await
}

/// Render one 10ms stereo chunk (960 samples) from a node's speaker.
pub fn render_tick(node: &mut VirtualNode) {
    let mut out = vec![0f32; 960];
    node.mixer.fill_buffer(&mut out);
    node.speaker_history.extend(out);
}

/// Drive a mesh: each tick transfers every edge in order, then renders every
/// node once (interleaved feed/fill, matching the realtime pattern).
pub async fn run_mesh(
    nodes: &mut HashMap<String, VirtualNode>,
    edges: &[(String, String)],
    ticks: usize,
) {
    let timeout = mesh_timeout(ticks, edges.len());
    run_mesh_timeout(nodes, edges, ticks, timeout).await;
}

pub async fn run_mesh_timeout(
    nodes: &mut HashMap<String, VirtualNode>,
    edges: &[(String, String)],
    ticks: usize,
    timeout: Duration,
) {
    let res = tokio::time::timeout(timeout, async {
        for _ in 0..ticks {
            for (tx, rx) in edges.iter() {
                let _ = tokio::time::timeout(TRANSFER_TIMEOUT, transfer_tick(nodes, tx, rx)).await;
            }
            let names: Vec<String> = nodes.keys().cloned().collect();
            for name in names.iter() {
                render_tick(nodes.get_mut(name).expect("test node"));
            }
        }
    })
    .await;
    assert!(
        res.is_ok(),
        "run_mesh timed out after {timeout:?} for {ticks} ticks x {} edges",
        edges.len()
    );
}

pub async fn transfer_tick_timeout(
    nodes: &mut HashMap<String, VirtualNode>,
    tx_name: &str,
    rx_name: &str,
) -> bool {
    tokio::time::timeout(TRANSFER_TIMEOUT, transfer_tick(nodes, tx_name, rx_name))
        .await
        .unwrap_or_default()
}

/// Loudness of a sample window (0..1 scale).
pub fn loudness(samples: &[f32]) -> f64 {
    calculate_loudness(samples)
}

/// Energy ratio `actual / reference` (1.0 = identical power).
pub fn energy_ratio(actual: &[f32], reference: &[f32]) -> f64 {
    assert_eq!(actual.len(), reference.len());
    let e_ref: f64 = reference.iter().map(|v| (*v as f64).powi(2)).sum();
    let e_act: f64 = actual.iter().map(|v| (*v as f64).powi(2)).sum();
    e_act / e_ref.max(1e-12)
}

/// Max normalized cross-correlation of `actual` vs `reference` over ±`max_lag`
/// samples. 1.0 = same waveform up to delay/gain; 0 = unrelated (or
/// phase-inverted; one-sided floor by design). This is the
/// primary "substantially the same waveform" gate: it tolerates Opus codec
/// delay that would destroy a naive sample-wise SNR.
pub fn max_normalized_xcorr(actual: &[f32], reference: &[f32], max_lag: usize) -> f64 {
    assert_eq!(actual.len(), reference.len());
    let n = actual.len();
    let lag = max_lag.min(n.saturating_sub(1));
    let mut best: f64 = 0.0;
    for shift in 0..=lag {
        for (a, r) in [(actual, reference), (reference, actual)] {
            let len = n - shift;
            let mut dot = 0.0;
            let mut ea = 0.0;
            let mut er = 0.0;
            for i in 0..len {
                let x = a[i + shift] as f64;
                let y = r[i] as f64;
                dot += x * y;
                ea += x * x;
                er += y * y;
            }
            let denom = (ea * er).sqrt().max(1e-12);
            best = best.max(dot / denom);
        }
    }
    best
}

/// Narrowband energy at `freq` via Goertzel (single-sided, arbitrary scale —
/// compare relatively between speakers/frequencies, not absolutely).
/// Uses every sample at the true sample rate (stereo channels just scale
/// energy equally, which cancels in relative comparisons).
pub fn goertzel_energy(samples: &[f32], freq: f32, sr: f32) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq as f64 / sr as f64;
    let (cw, sw) = (w.cos(), w.sin());
    let (mut u1, mut u2) = (0.0f64, 0.0f64);
    for &s in samples.iter() {
        let u0 = s as f64 + 2.0 * cw * u1 - u2;
        u2 = u1;
        u1 = u0;
    }
    let real = u1 * cw - u2;
    let imag = u1 * sw;
    real * real + imag * imag
}
