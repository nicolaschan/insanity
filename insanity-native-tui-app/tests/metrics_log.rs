use insanity_native_tui_app::audio::{AudioMixer, MixerMetricsSnapshot, format_metrics_line};
use insanity_native_tui_app::processor::{AudioChunk, AudioFormat};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize},
};

fn snapshot(
    gap_detected: usize,
    late_dropped: usize,
    underrun: usize,
    plc_hold: usize,
    clip_hits: usize,
    fills: usize,
) -> MixerMetricsSnapshot {
    MixerMetricsSnapshot {
        gap_detected,
        late_dropped,
        underrun,
        plc_hold,
        clip_hits,
        fills,
    }
}

#[test]
fn line_reports_all_counters_as_interval_deltas() {
    let prev = snapshot(1, 2, 3, 4, 5, 100);
    let current = snapshot(4, 6, 9, 12, 7, 200);
    let line = format_metrics_line(&prev, &current, 1234, &[]);
    assert!(line.contains("gaps=3"), "{line}");
    assert!(line.contains("late=4"), "{line}");
    assert!(line.contains("underruns=6"), "{line}");
    assert!(line.contains("plc=8"), "{line}");
    assert!(line.contains("clips=2"), "{line}");
    assert!(line.contains("fills=100"), "{line}");
    assert!(line.contains("fill_avg_ns=1234"), "{line}");
    assert!(line.contains("peers=[]"), "{line}");
}

#[test]
fn line_reports_occupancy_and_zero_delta() {
    let snap = snapshot(0, 0, 0, 0, 0, 0);
    let line = format_metrics_line(
        &snap,
        &snap,
        0,
        &[("peer-a".to_string(), 2), ("peer-b".to_string(), 0)],
    );
    assert!(line.contains("gaps=0"), "{line}");
    assert!(line.contains("underruns=0"), "{line}");
    assert!(line.contains("peers=[peer-a:2 peer-b:0]"), "{line}");
}

#[test]
fn line_saturates_on_counter_reset() {
    let prev = snapshot(10, 0, 0, 0, 0, 0);
    let current = snapshot(3, 0, 0, 0, 0, 0);
    let line = format_metrics_line(&prev, &current, 0, &[]);
    assert!(line.contains("gaps=0"), "{line}");
}

#[test]
fn occupancies_reflect_buffered_chunks() {
    let mixer = AudioMixer::new_no_device();
    assert!(mixer.peer_occupancies().is_empty());
    let id = uuid::Uuid::new_v4();
    mixer.add_peer(
        id,
        Arc::new(AtomicUsize::new(100)),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    mixer.handle_incoming(
        id,
        AudioChunk::new(0, AudioFormat::new(2, 48000), vec![0.1; 960]),
    );
    let occupancies = mixer.peer_occupancies();
    assert_eq!(occupancies.len(), 1);
    assert_eq!(occupancies[0].0, id.to_string());
    assert_eq!(occupancies[0].1, 1);
    let line = format_metrics_line(
        &MixerMetricsSnapshot::default(),
        &mixer.metrics_snapshot(),
        mixer.fill_avg_nanos(),
        &occupancies,
    );
    assert!(line.contains(&format!("{}:1", id)), "{line}");
}
