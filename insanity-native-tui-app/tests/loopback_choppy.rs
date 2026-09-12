#[path = "common/audio_math.rs"]
mod audio_math;
#[path = "common/mesh.rs"]
mod mesh;
#[path = "common/sine.rs"]
mod sine;
#[path = "common/unit_mixer.rs"]
mod unit_mixer;

use audio_math::{energy_ratio, loudness, max_normalized_xcorr, tail};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::codec::AudioEncoder;
use insanity_core::audio::jitter::JitterBuffer;
use insanity_core::audio::mixer::Mixer;
use insanity_core::audio::sample::{SampleSource, SyncSampleSource};
use insanity_core::audio::transform::Gain;
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::codec::OpusEncoder;
use insanity_native_tui_app::audio::mixer::{PeerControls, chain_from_controls, output_resampler};
use insanity_native_tui_app::audio::params::MAX_VOLUME;
use mesh::{VirtualNode, render_tick, run_mesh, transfer_tick_timeout};
use opus::{Channels, Decoder};
use sine::{SineSource, decode_frame_to_chunk, hub_from_source};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use unit_mixer::{
    UnitMixer, add_unit_peer, push_chunk, push_value, rebuild_passthrough, render, unit_mixer,
};

struct ChirpSource {
    n: u64,
    sr: u32,
    amp: f32,
    total: u64,
    phase: f64,
}

impl ChirpSource {
    fn new(sr: u32, amp: f32, total: u64) -> Self {
        Self {
            n: 0,
            sr,
            amp,
            total,
            phase: 0.0,
        }
    }

    fn step(&mut self) -> f32 {
        let t = self.n as f64 / self.total.max(1) as f64;
        let freq = 200.0 + 1800.0 * t;
        self.phase += freq / self.sr as f64;
        self.n += 1;
        ((self.phase * 2.0 * std::f64::consts::PI).sin() as f32) * self.amp
    }
}

impl SampleSource for ChirpSource {
    async fn next(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

impl SyncSampleSource for ChirpSource {
    fn next_sync(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

struct AmSpeechSource {
    n: u64,
    sr: u32,
    amp: f32,
    phase_a: f64,
    phase_b: f64,
}

impl AmSpeechSource {
    fn new(sr: u32, amp: f32) -> Self {
        Self {
            n: 0,
            sr,
            amp,
            phase_a: 0.0,
            phase_b: 0.0,
        }
    }

    fn step(&mut self) -> f32 {
        let t = self.n as f64 / self.sr as f64;
        self.phase_a += 440.0 / self.sr as f64;
        self.phase_b += 880.0 / self.sr as f64;
        self.n += 1;
        let am = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
        let v = (self.phase_a * 2.0 * std::f64::consts::PI).sin() * 0.7
            + (self.phase_b * 2.0 * std::f64::consts::PI).sin() * 0.3;
        (v as f32) * (am as f32) * (self.amp / 0.5) * 0.5
    }
}

impl SampleSource for AmSpeechSource {
    async fn next(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

impl SyncSampleSource for AmSpeechSource {
    fn next_sync(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

fn music_pair() -> HashMap<String, VirtualNode> {
    let mut nodes = HashMap::new();
    let mut a = VirtualNode::with_source(
        "a",
        AmSpeechSource::new(48000, 0.5),
        AudioFormat::new(2, 48000),
    );
    let mut b = VirtualNode::with_source(
        "b",
        ChirpSource::new(48000, 0.0, 1),
        AudioFormat::new(2, 48000),
    );
    a.add_outbound("b");
    b.add_inbound("a");
    nodes.insert("a".to_string(), a);
    nodes.insert("b".to_string(), b);
    nodes
}

#[tokio::test]
async fn stereo_music_clean_loopback_no_gaps() {
    let timeout = Duration::from_secs(60);
    let res = tokio::time::timeout(timeout, async {
        let mut nodes = music_pair();
        let edges = vec![("a".to_string(), "b".to_string())];
        run_mesh(&mut nodes, &edges, 40).await;
        assert_eq!(nodes["b"].speaker_history.len(), 40 * 960);
        let snap = nodes["b"].metrics_snapshot();
        assert_eq!(
            snap.gap_detected, 0,
            "clean loopback must not gap: {snap:?}"
        );
        assert_eq!(
            snap.underrun, 0,
            "clean loopback must not underrun: {snap:?}"
        );
        let mic_tail = tail(&nodes["a"].mic_history, 20).to_vec();
        let spk_tail = tail(&nodes["b"].speaker_history, 20).to_vec();
        let xcorr = max_normalized_xcorr(&spk_tail, &mic_tail, 960);
        assert!(
            xcorr > 0.7,
            "music survives clean loopback, xcorr {xcorr:.3}"
        );
        let dl = (loudness(&spk_tail) - loudness(&mic_tail)).abs();
        assert!(dl < 0.1, "music loudness drift {dl:.3}");
        let er = energy_ratio(&spk_tail, &mic_tail);
        assert!((0.3..3.0).contains(&er), "music energy ratio {er:.3}");
    })
    .await;
    assert!(res.is_ok(), "stereo_music_clean_loopback_no_gaps timed out");
}

#[tokio::test]
async fn non48k_input_resample_loopback() {
    let timeout = Duration::from_secs(60);
    let res = tokio::time::timeout(timeout, async {
        let mut nodes = HashMap::new();
        let mut a = VirtualNode::with_source(
            "a",
            SineSource::new_amp(44100, 440.0, 0.5),
            AudioFormat::new(2, 44100),
        );
        let mut b = VirtualNode::with_source(
            "b",
            SineSource::new_amp(48000, 880.0, 0.0),
            AudioFormat::new(2, 48000),
        );
        a.add_outbound("b");
        b.add_inbound("a");
        nodes.insert("a".to_string(), a);
        nodes.insert("b".to_string(), b);
        let edges = vec![("a".to_string(), "b".to_string())];
        run_mesh(&mut nodes, &edges, 40).await;
        assert_eq!(nodes["b"].speaker_history.len(), 40 * 960);
        assert!(
            !nodes["a"].mic_history.is_empty(),
            "resampled mic must produce audio"
        );
        let mut reference = SineSource::new_amp(48000, 440.0, 0.5);
        let expected: Vec<f32> = (0..20 * 960)
            .map(|_| reference.next_sync().expect("sine"))
            .collect();
        let spk_tail = tail(&nodes["b"].speaker_history, 20).to_vec();
        let xcorr = max_normalized_xcorr(&spk_tail, &expected, 960);
        assert!(
            xcorr > 0.7,
            "44100 sine survives resample loopback {xcorr:.3}"
        );
        let snap = nodes["b"].metrics_snapshot();
        assert_eq!(
            snap.underrun, 0,
            "resampled loopback must not underrun: {snap:?}"
        );
    })
    .await;
    assert!(res.is_ok(), "non48k_input_resample_loopback timed out");
}

#[test]
fn resampled_output_fill_budget() {
    let out_format = AudioFormat::new(2, 44100);
    let (bus, _) = Gain::shared(100, MAX_VOLUME);
    let mut mixer: UnitMixer = Mixer::new(out_format.clone(), 10, 480, bus);
    let controls = PeerControls::new(100, DenoiseSelection::None);
    let chain = chain_from_controls(&controls);
    let id = mixer.subscribe(
        chain,
        rebuild_passthrough,
        output_resampler(out_format, 480),
    );
    let push = |mixer: &mut UnitMixer, seq: u128| {
        push_chunk(
            mixer,
            id,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), vec![0.4; 960]),
        );
    };
    for seq in 0..3u128 {
        push(&mut mixer, seq);
    }
    let start = std::time::Instant::now();
    for seq in 3..13u128 {
        let out = render(&mut mixer, 960);
        for s in out.iter() {
            assert!(s.is_finite());
            assert!(s.abs() <= 1.0 + 1e-6);
        }
        push(&mut mixer, seq);
    }
    let elapsed = start.elapsed();
    let snap = mixer.metrics_snapshot();
    assert_eq!(
        snap.underrun, 0,
        "prefilled resampled mixer must not underrun: {snap:?}"
    );
    let realtime_nanos = 960 / 2 * 1_000_000_000 / 44_100;
    let avg_nanos = elapsed.as_nanos() as u64 / 10;
    assert!(
        avg_nanos < realtime_nanos,
        "fill budget blown: {avg_nanos}ns of {realtime_nanos}ns"
    );
}

#[tokio::test]
async fn broadcast_lag_records_gap() {
    let res = tokio::time::timeout(Duration::from_secs(15), async {
        let hub = Arc::new(hub_from_source(
            SineSource::new_amp(48000, 440.0, 0.5),
            AudioFormat::new(2, 48000),
        ));
        let mut lagging = hub.subscribe();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let first = tokio::time::timeout(Duration::from_secs(2), lagging.recv())
            .await
            .expect("lagging recv timeout");
        let jumped_seq = match first {
            Ok(chunk) => chunk.sequence_number,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let inner = tokio::time::timeout(Duration::from_secs(2), lagging.recv())
                    .await
                    .expect("post-lag recv timeout");
                inner.expect("hub closed").sequence_number
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => panic!("hub closed"),
        };
        assert!(
            jumped_seq > 0,
            "lagging receiver must observe a seq jump, got {jumped_seq}"
        );
        let (mut mixer, _) = unit_mixer(100);
        let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
        push_value(&mut mixer, id, jumped_seq, 0.1);
        push_value(&mut mixer, id, 0, 0.1);
        let snap = mixer.metrics_snapshot();
        assert!(
            snap.gap_detected > 0,
            "seq jump must be recorded, got {snap:?}"
        );
        let mut buf = JitterBuffer::new(3);
        buf.set(0, 0);
        buf.set(jumped_seq, 1);
        assert_eq!(
            buf.head(),
            jumped_seq - 2,
            "buffer must fast-forward, head {}",
            buf.head()
        );
    })
    .await;
    assert!(res.is_ok(), "broadcast_lag_records_gap timed out");
}

#[tokio::test]
async fn burst_loss_still_realtime() {
    let timeout = Duration::from_secs(60);
    let res = tokio::time::timeout(timeout, async {
        let mut nodes = HashMap::new();
        let mut a = VirtualNode::new("a", 440.0);
        let mut b = VirtualNode::new("b", 880.0);
        a.add_outbound("b");
        b.add_inbound("a");
        nodes.insert("a".to_string(), a);
        nodes.insert("b".to_string(), b);
        for t in 0..40 {
            if !(20..23).contains(&t) {
                assert!(transfer_tick_timeout(&mut nodes, "a", "b").await);
            } else {
                let tx = nodes.get_mut("a").expect("node");
                let _ = tx.pull_frame("b").await;
            }
            let names: Vec<String> = nodes.keys().cloned().collect();
            for name in names.iter() {
                render_tick(nodes.get_mut(name).expect("node"));
            }
        }
        assert_eq!(nodes["b"].speaker_history.len(), 40 * 960);
        let snap = nodes["b"].metrics_snapshot();
        assert!(snap.gap_detected > 0, "burst must be recorded: {snap:?}");
        assert!(snap.underrun > 0, "burst must conceal: {snap:?}");
        let mic_tail = tail(&nodes["a"].mic_history, 10).to_vec();
        let spk_tail = tail(&nodes["b"].speaker_history, 10).to_vec();
        let xcorr = max_normalized_xcorr(&spk_tail, &mic_tail, 960);
        assert!(xcorr > 0.7, "post-burst recovery xcorr {xcorr:.3}");
        let dl = (loudness(&spk_tail) - loudness(&mic_tail)).abs();
        assert!(dl < 0.1, "post-burst loudness drift {dl:.3}");
    })
    .await;
    assert!(res.is_ok(), "burst_loss_still_realtime timed out");
}

#[test]
fn opus_stereo_frame_roundtrip_shape() {
    let mut enc = OpusEncoder::new(48000, 2).expect("encoder");
    let mut dec = Decoder::new(48000, Channels::Stereo).expect("decoder");
    let mut out = None;
    for seq in 0..6u128 {
        let chunk: Vec<f32> = (0..480)
            .flat_map(|i| {
                let n = seq as usize * 480 + i;
                let s = (n as f32 * 440.0 / 48000.0 * 2.0 * std::f32::consts::PI).sin() * 0.4;
                vec![s, s]
            })
            .collect();
        let frame = enc
            .encode(&AudioChunk::new(
                seq,
                AudioFormat::new(2, 48000),
                chunk.clone(),
            ))
            .expect("encode");
        let decoded = decode_frame_to_chunk(&mut dec, &frame).expect("decode");
        assert_eq!(decoded.sequence_number, seq);
        assert_eq!(decoded.audio_data.len(), 960);
        out = Some((decoded.audio_data, chunk));
    }
    let (decoded, chunk) = out.expect("frames");
    let xcorr = max_normalized_xcorr(&decoded, &chunk, 960);
    assert!(xcorr > 0.8, "stereo frame survives, xcorr {xcorr:.3}");
}
