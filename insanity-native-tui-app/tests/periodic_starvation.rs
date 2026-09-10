use insanity_core::audio::{AudioFormat, chunk::AudioChunk, chunk::ChunkSource};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::{AudioInputHub, AudioMixer};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Duration;

fn window_rms(samples: &[f32]) -> f64 {
    let e: f64 = samples.iter().map(|v| (*v as f64).powi(2)).sum();
    (e / samples.len().max(1) as f64).sqrt()
}

fn count_dips(samples: &[f32], window: usize, thresh_db: f64) -> usize {
    let mut energies = Vec::new();
    for w in samples.chunks(window) {
        if w.len() == window {
            energies.push(window_rms(w));
        }
    }
    let median = {
        let mut s = energies.clone();
        s.sort_by(|a, b| a.partial_cmp(b).expect("rms"));
        s[s.len() / 2]
    };
    energies
        .iter()
        .filter(|e| 20.0 * (**e / median.max(1e-9)).log10() < -thresh_db)
        .count()
}

#[test]
fn empty_buffer_4096_matches_log_signature() {
    let mixer = AudioMixer::new_no_device();
    let id = uuid::Uuid::new_v4();
    mixer.add_peer(
        id,
        Arc::new(AtomicUsize::new(100)),
        Arc::new(std::sync::Mutex::new(DenoiseSelection::None)),
        None,
    );
    let fills = 10usize;
    let callback = 4096usize;
    for _ in 0..fills {
        let mut out = vec![0f32; callback];
        mixer.fill_buffer(&mut out);
    }
    let snap = mixer.metrics_snapshot();
    assert_eq!(snap.gap_detected, 0, "no feed means no seq jumps: {snap:?}");
    assert_eq!(snap.late_dropped, 0, "{snap:?}");
    assert!(snap.underrun > 0, "empty buffer must underrun: {snap:?}");
    let plc_per_fill = snap.plc_hold as f64 / snap.fills as f64;
    assert!(
        (plc_per_fill - callback as f64).abs() < 1.0,
        "every sample concealed when fully starved: {snap:?}"
    );
    let underruns_per_fill = snap.underrun as f64 / snap.fills as f64;
    assert!(
        (underruns_per_fill - callback as f64 / 960.0).abs() < 1.0,
        "one underrun event per 960-sample fade run: {snap:?}"
    );
    assert_eq!(mixer.peer_occupancy(&id), Some(0));
}

#[test]
fn sustained_960_with_steady_feed_stays_clean() {
    let mixer = AudioMixer::new_no_device();
    let id = uuid::Uuid::new_v4();
    mixer.add_peer(
        id,
        Arc::new(AtomicUsize::new(100)),
        Arc::new(std::sync::Mutex::new(DenoiseSelection::None)),
        None,
    );
    let mut seq: u128 = 0;
    for _ in 0..10 {
        mixer.handle_incoming(
            id,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), vec![0.4; 960]),
        );
        seq += 1;
    }
    for _ in 0..30 {
        let mut out = vec![0f32; 960];
        mixer.fill_buffer(&mut out);
        for s in out.iter() {
            assert!(s.is_finite());
        }
        mixer.handle_incoming(
            id,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), vec![0.4; 960]),
        );
        seq += 1;
    }
    let snap = mixer.metrics_snapshot();
    assert_eq!(snap.gap_detected, 0, "{snap:?}");
    assert_eq!(snap.underrun, 0, "{snap:?}");
    assert_eq!(snap.fills, 30);
}

#[test]
fn fully_starved_output_shows_repeated_dips() {
    let mixer = AudioMixer::new_no_device();
    let id = uuid::Uuid::new_v4();
    mixer.add_peer(
        id,
        Arc::new(AtomicUsize::new(100)),
        Arc::new(std::sync::Mutex::new(DenoiseSelection::None)),
        None,
    );
    mixer.handle_incoming(
        id,
        AudioChunk::new(0, AudioFormat::new(2, 48000), vec![0.5; 960]),
    );
    let mut out = Vec::new();
    for _ in 0..20 {
        let mut buf = vec![0f32; 960];
        mixer.fill_buffer(&mut buf);
        out.extend(buf);
    }
    let dips = count_dips(&out, 960, 10.0);
    assert!(dips > 0, "repeated PLC fades must read as periodic dips");
}

struct BurstySource {
    seq: u128,
    phase: f32,
}

impl BurstySource {
    fn chunk(&mut self) -> AudioChunk {
        let mut data = Vec::with_capacity(960);
        for _ in 0..480 {
            let v = (self.phase * 2.0 * std::f32::consts::PI).sin() * 0.4;
            self.phase = (self.phase + 440.0 / 48000.0) % 1.0;
            data.push(v);
            data.push(v);
        }
        let chunk = AudioChunk::new(self.seq, AudioFormat::new(2, 48000), data);
        self.seq += 1;
        chunk
    }
}

impl ChunkSource for BurstySource {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        if self.seq.is_multiple_of(4) && self.seq != 0 {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        Some(self.chunk())
    }
}

#[tokio::test]
async fn bursty_source_is_paced_to_10ms() {
    let hub = AudioInputHub::from_chunk_source(BurstySource { seq: 0, phase: 0.0 });
    let mut rx = hub.subscribe();
    let mut times = Vec::new();
    for _ in 0..12 {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("frame")
            .expect("hub open");
        times.push(std::time::Instant::now());
    }
    let mut gaps: Vec<f64> = times
        .windows(2)
        .map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0)
        .collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).expect("inter-arrival"));
    let median = gaps[gaps.len() / 2];
    let bursts = gaps.iter().filter(|g| **g < 5.0).count();
    let burst_frac = bursts as f64 / gaps.len() as f64;
    assert!(
        burst_frac < 0.25,
        "hub must smooth bursts, got burst_frac {burst_frac:.2} gaps_ms={gaps:?}"
    );
    assert!(
        (7.0..20.0).contains(&median),
        "hub must pace to ~10ms, got median {median:.2}ms gaps_ms={gaps:?}"
    );
}
