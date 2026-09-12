#![allow(dead_code)]
use crate::sine::{decode_frame_to_chunk, hub_from_source, new_no_device_mixer, opus_channels};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::codec::EncodedChunk;
use insanity_core::audio::mixer::DEFAULT_OUT_FRAMES;
use insanity_core::audio::mixer::{MixerMetrics, SlotId};
use insanity_core::audio::sample::{SampleSource, SyncSampleSource};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::{
    AppMixer, AudioInputHub, PeerControls, chain_from_controls, output_resampler,
    rebuild_opus_decoder,
};
use insanity_native_tui_app::processor::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use insanity_native_tui_app::protocol::ProtocolMessage;
use opus::Decoder;
use std::collections::HashMap;
use std::sync::Arc;
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

pub struct VirtualNode {
    hub: Arc<AudioInputHub>,
    mixer: AppMixer,
    peer_ids: HashMap<String, SlotId>,
    peer_controls: HashMap<String, PeerControls>,
    out_samples: usize,
    hub_taps: HashMap<String, broadcast::Receiver<EncodedChunk>>,
    monitor: Decoder,
    monitor_format: AudioFormat,
    pub mic_history: Vec<f32>,
    mic_last_seq: Option<u128>,
    pub speaker_history: Vec<f32>,
}

impl VirtualNode {
    pub fn new(_name: &str, freq: f32) -> Self {
        Self::with_amp(_name, freq, 0.5)
    }

    pub fn with_amp(_name: &str, freq: f32, amp: f32) -> Self {
        let format = AudioFormat::new(2, AUDIO_SAMPLE_RATE);
        Self::with_source(
            _name,
            crate::sine::SineSource::new_amp(AUDIO_SAMPLE_RATE, freq, amp),
            format,
        )
    }

    pub fn with_source<S>(_name: &str, source: S, format: AudioFormat) -> Self
    where
        S: SampleSource + Send + Sync + 'static,
    {
        let hub = Arc::new(hub_from_source(source, format.clone()));
        Self {
            hub_taps: HashMap::new(),
            mixer: new_no_device_mixer(),
            peer_ids: HashMap::new(),
            peer_controls: HashMap::new(),
            out_samples: DEFAULT_OUT_FRAMES * AUDIO_CHANNELS as usize,
            monitor: Decoder::new(AUDIO_SAMPLE_RATE, opus_channels(format.channel_count))
                .expect("monitor decoder"),
            monitor_format: AudioFormat::new(format.channel_count, AUDIO_SAMPLE_RATE),
            mic_history: Vec::new(),
            mic_last_seq: None,
            speaker_history: Vec::new(),
            hub,
        }
    }

    pub fn add_inbound(&mut self, peer_name: &str) {
        self.add_inbound_denoise(peer_name, DenoiseSelection::None);
    }

    pub fn add_inbound_denoise(
        &mut self,
        peer_name: &str,
        denoise: DenoiseSelection,
    ) -> PeerControls {
        let controls = PeerControls::new(100, denoise);
        let chain = chain_from_controls(&controls);
        let slot = self.mixer.subscribe(
            chain,
            rebuild_opus_decoder,
            output_resampler(AudioFormat::new(2, AUDIO_SAMPLE_RATE), DEFAULT_OUT_FRAMES),
        );
        self.peer_ids.insert(peer_name.to_string(), slot);
        self.peer_controls
            .insert(peer_name.to_string(), controls.clone());
        controls
    }

    pub fn add_outbound(&mut self, peer_name: &str) {
        self.hub_taps
            .insert(peer_name.to_string(), self.hub.subscribe());
    }

    pub fn set_muted(&self, muted: bool) {
        self.hub.set_muted(muted);
    }

    pub fn metrics_snapshot(&self) -> MixerMetrics {
        self.mixer.metrics_snapshot()
    }

    pub async fn pull_frame(&mut self, peer_name: &str) -> Option<Vec<u8>> {
        let tap = self.hub_taps.get_mut(peer_name)?;
        let frame = match tokio::time::timeout(PULL_TIMEOUT, tap.recv()).await {
            Ok(Ok(f)) => f,
            Ok(Err(_)) | Err(_) => return None,
        };
        let seq = frame.sequence_number;
        if self.mic_last_seq != Some(seq) {
            self.mic_last_seq = Some(seq);
            if frame.format != self.monitor_format {
                let Ok(monitor) = Decoder::new(
                    frame.format.sample_rate,
                    opus_channels(frame.format.channel_count),
                ) else {
                    return None;
                };
                self.monitor = monitor;
                self.monitor_format = frame.format.clone();
            }
            let decoded = decode_frame_to_chunk(&mut self.monitor, &frame)?;
            self.mic_history.extend(decoded.audio_data);
        }
        let mut buf = Vec::new();
        ProtocolMessage::Encoded(frame)
            .write_to_stream(&mut buf)
            .await
            .ok()?;
        Some(buf)
    }

    pub async fn push_frame(&mut self, peer_name: &str, bytes: &[u8]) -> bool {
        let Ok(ProtocolMessage::Encoded(frame)) =
            ProtocolMessage::read_from_stream(&mut &bytes[..]).await
        else {
            return false;
        };
        let Some(slot) = self.peer_ids.get(peer_name).copied() else {
            return false;
        };
        self.mixer.push_to_slot(slot, frame)
    }
}

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

pub fn render_tick(node: &mut VirtualNode) {
    let count = node.out_samples;
    let out: Vec<f32> = (0..count)
        .map(|_| node.mixer.next_sync().unwrap_or(0.0))
        .collect();
    node.speaker_history.extend(out);
}

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
