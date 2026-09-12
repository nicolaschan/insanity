use insanity_core::audio::{AudioFormat, chunk::AudioChunk};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio_test_support::{
    UnitMixer, add_unit_peer, push_chunk, render, unit_mixer_with_jitter,
};

// Feed model: each fill pushes floor(callback/960) whole chunks, so
// non-multiple callback sizes (e.g. 2048 -> 2 chunks = 1920 samples)
// carry a systematic underfeed on top of any capacity shortfall;
// compare capacities within a callback column, not across them.
// `underrun_events_per_sample` divides event counts by samples and is
// only order-of-magnitude comparable for the same reason.

struct CellResult {
    callback: usize,
    capacity: usize,
    condition: &'static str,
    underruns: usize,
    fills: usize,
    gaps: usize,
    late: usize,
    clips: usize,
    fill_avg_ns: u64,
}

fn mixer_with_capacity(chunks: usize) -> UnitMixer {
    unit_mixer_with_jitter(100, chunks).0
}

fn run_cell(callback: usize, capacity: usize, condition: &'static str) -> CellResult {
    let mut mixer = mixer_with_capacity(capacity);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    let feed_per_fill = callback / 960;
    let mut next_seq: u128 = 0;
    let push = |mixer: &mut UnitMixer, next_seq: &mut u128, count: usize| {
        for _ in 0..count {
            push_chunk(
                mixer,
                id,
                AudioChunk::new(*next_seq, AudioFormat::new(2, 48000), vec![0.4; 960]),
            );
            *next_seq += 1;
        }
    };
    push(&mut mixer, &mut next_seq, capacity);
    let fills = 30;
    let mut total_nanos: u64 = 0;
    for t in 0..fills {
        let stalled = condition == "stall3" && (12..15).contains(&t);
        let start = std::time::Instant::now();
        let _ = render(&mut mixer, callback);
        total_nanos += start.elapsed().as_nanos() as u64;
        if !stalled {
            push(&mut mixer, &mut next_seq, feed_per_fill);
        }
    }
    let snap = mixer.metrics_snapshot();
    CellResult {
        callback,
        capacity,
        condition,
        underruns: snap.underrun,
        fills: snap.fills,
        gaps: snap.gap_detected,
        late: snap.late_dropped,
        clips: snap.clip_hits,
        fill_avg_ns: total_nanos / fills as u64,
    }
}

fn main() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target/buffer_sweep/runs.csv");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let mut csv = String::from(
        "callback,capacity,condition,underruns,fills,underrun_events_per_sample,gaps,late,clips,fill_avg_ns\n",
    );
    let mut worst: Option<CellResult> = None;
    for &callback in &[960usize, 2048, 4100] {
        for &capacity in &[3usize, 5, 10, 20, 30] {
            for &condition in &["clean", "stall3"] {
                let r = run_cell(callback, capacity, condition);
                let rate = r.underruns as f64 / (r.fills * r.callback).max(1) as f64;
                csv.push_str(&format!(
                    "{},{},{},{},{},{:.4},{},{},{},{}\n",
                    r.callback,
                    r.capacity,
                    r.condition,
                    r.underruns,
                    r.fills,
                    rate,
                    r.gaps,
                    r.late,
                    r.clips,
                    r.fill_avg_ns
                ));
                let worse = match &worst {
                    None => true,
                    Some(w) => rate > w.underruns as f64 / (w.fills * w.callback).max(1) as f64,
                };
                if worse {
                    worst = Some(r);
                }
            }
        }
    }
    std::fs::write(&path, csv).expect("write csv");
    if let Some(w) = worst {
        eprintln!(
            "buffer_sweep: worst callback={} capacity={} {} underruns={} fills={} -> {}",
            w.callback,
            w.capacity,
            w.condition,
            w.underruns,
            w.fills,
            path.display()
        );
    }
}
