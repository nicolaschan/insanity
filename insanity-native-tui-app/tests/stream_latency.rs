use insanity_core::audio::{AudioFormat, chunk::AudioChunk};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::AudioMixer;
use std::sync::{Arc, atomic::AtomicUsize};

struct CellResult {
    callback_frames: usize,
    underruns: usize,
    gaps: usize,
    fills: usize,
    mean_occupancy: f64,
    est_queue_latency_ms: f64,
}

fn run_cell(callback_frames: usize, bursty: bool) -> CellResult {
    let mixer = AudioMixer::new_no_device_with_format_and_capacity(48000, 2, 10);
    let id = uuid::Uuid::new_v4();
    mixer.add_peer(
        id,
        Arc::new(AtomicUsize::new(100)),
        Arc::new(std::sync::Mutex::new(DenoiseSelection::None)),
        None,
    );
    let callback_samples = callback_frames * 2;
    let mut next_seq: u128 = 0;
    let mut pending_samples: Vec<f32> = Vec::new();
    let mut push_samples = |mixer: &AudioMixer, pending: &mut Vec<f32>, count: usize| {
        pending.extend(std::iter::repeat_n(0.4, count));
        while pending.len() >= 960 {
            let data: Vec<f32> = pending.drain(..960).collect();
            mixer.handle_incoming(
                id,
                AudioChunk::new(next_seq, AudioFormat::new(2, 48000), data),
            );
            next_seq += 1;
        }
    };
    for _ in 0..5 {
        push_samples(&mixer, &mut pending_samples, 960);
    }
    let total_samples = 5 * 48000 * 2;
    let fills = total_samples / callback_samples;
    let mut occ_sum = 0usize;
    let burst_every = (48000 / 10 * 2) / callback_samples;
    for t in 0..fills {
        let mut out = vec![0f32; callback_samples];
        mixer.fill_buffer(&mut out);
        for s in out.iter() {
            assert!(s.is_finite());
        }
        if bursty {
            if t % burst_every.max(1) == 0 {
                push_samples(
                    &mixer,
                    &mut pending_samples,
                    callback_samples * burst_every.max(1),
                );
            }
        } else {
            push_samples(&mixer, &mut pending_samples, callback_samples);
        }
        occ_sum += mixer.peer_occupancy(&id).unwrap_or(usize::MAX);
    }
    let snap = mixer.metrics_snapshot();
    let mean_occupancy = occ_sum as f64 / fills.max(1) as f64;
    let callback_ms = callback_frames as f64 / 48.0;
    CellResult {
        callback_frames,
        underruns: snap.underrun,
        gaps: snap.gap_detected,
        fills: snap.fills,
        mean_occupancy,
        est_queue_latency_ms: mean_occupancy * 10.0 + callback_ms / 2.0,
    }
}

#[test]
fn callback_size_latency_table() {
    for bursty in [false, true] {
        eprintln!(
            "feed={} callback_frames | underruns | gaps | mean_occ | est_queue_latency_ms",
            if bursty { "bursty" } else { "steady" }
        );
        let mut results = Vec::new();
        for &frames in &[480usize, 960, 1920, 2048, 4096] {
            let r = run_cell(frames, bursty);
            eprintln!(
                "{} | {} | {} | {:.2} | {:.1}",
                r.callback_frames, r.underruns, r.gaps, r.mean_occupancy, r.est_queue_latency_ms
            );
            results.push(r);
        }
        for r in results.iter() {
            if !bursty && r.callback_frames.is_multiple_of(480) {
                assert_eq!(
                    r.underruns, 0,
                    "callback {} under steady feed must not underrun: fills={} gaps={}",
                    r.callback_frames, r.fills, r.gaps
                );
            }
        }
        let first = results.first().expect("results").est_queue_latency_ms;
        let last = results.last().expect("results").est_queue_latency_ms;
        assert!(
            first < last,
            "smaller callbacks must estimate lower queue latency: {first:.1} vs {last:.1}"
        );
    }
}
