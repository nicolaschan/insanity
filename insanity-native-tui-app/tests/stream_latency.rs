#[path = "common/unit_mixer.rs"]
mod unit_mixer;

use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::user_input_event::DenoiseSelection;
use unit_mixer::{UnitMixer, add_unit_peer, assert_all_finite, push_chunk, render, unit_mixer};

struct CellResult {
    callback_frames: usize,
    underruns: usize,
    gaps: usize,
    fills: usize,
}

fn run_cell(callback_frames: usize, bursty: bool) -> CellResult {
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    let callback_samples = callback_frames * 2;
    let mut next_seq: u128 = 0;
    let mut pending_samples: Vec<f32> = Vec::new();
    let mut push_samples = |mixer: &mut UnitMixer, pending: &mut Vec<f32>, count: usize| {
        pending.extend(std::iter::repeat_n(0.4, count));
        while pending.len() >= 960 {
            let data: Vec<f32> = pending.drain(..960).collect();
            push_chunk(
                mixer,
                id,
                AudioChunk::new(next_seq, AudioFormat::new(2, 48000), data),
            );
            next_seq += 1;
        }
    };
    for _ in 0..5 {
        push_samples(&mut mixer, &mut pending_samples, 960);
    }
    let total_samples = 5 * 48000 * 2;
    let fills = total_samples / callback_samples;
    let burst_every = (48000 / 10 * 2) / callback_samples;
    for t in 0..fills {
        let out = render(&mut mixer, callback_samples);
        assert_all_finite(&out);
        if bursty {
            if t % burst_every.max(1) == 0 {
                push_samples(
                    &mut mixer,
                    &mut pending_samples,
                    callback_samples * burst_every.max(1),
                );
            }
        } else {
            push_samples(&mut mixer, &mut pending_samples, callback_samples);
        }
    }
    let snap = mixer.metrics_snapshot();
    CellResult {
        callback_frames,
        underruns: snap.underrun,
        gaps: snap.gap_detected,
        fills: snap.fills,
    }
}

#[test]
fn callback_size_latency_table() {
    for bursty in [false, true] {
        eprintln!(
            "feed={} callback_frames | underruns | gaps | fills",
            if bursty { "bursty" } else { "steady" }
        );
        let mut results = Vec::new();
        for &frames in &[480usize, 960, 1920, 2048, 4096] {
            let r = run_cell(frames, bursty);
            eprintln!(
                "{} | {} | {} | {}",
                r.callback_frames, r.underruns, r.gaps, r.fills
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
    }
}
