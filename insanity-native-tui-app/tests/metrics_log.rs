#[path = "common/unit_mixer.rs"]
mod unit_mixer;

use insanity_core::audio::mixer::MixerMetrics;
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::mixer::format_audio_interval;
use unit_mixer::{add_unit_peer, push_value, render, unit_mixer};

fn snapshot(
    gap_detected: usize,
    late_dropped: usize,
    underrun: usize,
    plc_hold: usize,
    clip_hits: usize,
    fills: usize,
) -> MixerMetrics {
    MixerMetrics {
        gap_detected,
        late_dropped,
        overflow_dropped: 0,
        underrun,
        plc_hold,
        clip_hits,
        fills,
        stale_dropped: 0,
    }
}

#[test]
fn line_reports_all_counters_as_interval_deltas() {
    let prev = snapshot(1, 2, 3, 4, 5, 100);
    let current = snapshot(4, 6, 9, 12, 7, 200);
    let line = format_audio_interval(&prev, &current, 1234, 0, 0, 0, 0);
    assert!(line.contains("gaps=3"), "{line}");
    assert!(line.contains("late=4"), "{line}");
    assert!(line.contains("overflow=0"), "{line}");
    assert!(line.contains("underruns=6"), "{line}");
    assert!(line.contains("plc=8"), "{line}");
    assert!(line.contains("clips=2"), "{line}");
    assert!(line.contains("fills=100"), "{line}");
    assert!(line.contains("stale=0"), "{line}");
    assert!(line.contains("fill_avg_ns=1234"), "{line}");
    assert!(line.contains("peers=0"), "{line}");
}

#[test]
fn line_reports_peer_count_and_zero_delta() {
    let snap = snapshot(0, 0, 0, 0, 0, 0);
    let line = format_audio_interval(&snap, &snap, 0, 2, 0, 0, 0);
    assert!(line.contains("gaps=0"), "{line}");
    assert!(line.contains("underruns=0"), "{line}");
    assert!(line.contains("peers=2"), "{line}");
}

#[test]
fn line_saturates_on_counter_reset() {
    let prev = snapshot(10, 0, 0, 0, 0, 0);
    let current = snapshot(3, 0, 0, 0, 0, 0);
    let line = format_audio_interval(&prev, &current, 0, 0, 0, 0, 0);
    assert!(line.contains("gaps=0"), "{line}");
}

#[test]
fn mixer_reports_peer_count_and_counters() {
    let (mut mixer, _) = unit_mixer(100);
    assert_eq!(mixer.peer_count(), 0);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    assert_eq!(mixer.peer_count(), 1);
    push_value(&mut mixer, id, 0, 0.1);
    let snap = mixer.metrics_snapshot();
    assert_eq!(snap.gap_detected, 0);
    let line = format_audio_interval(
        &MixerMetrics::default(),
        &snap,
        0,
        mixer.peer_count(),
        0,
        0,
        0,
    );
    assert!(line.contains("peers=1"), "{line}");
    assert!(line.contains("gaps=0"), "{line}");
    let _ = render(&mut mixer, 960);
    assert!(mixer.metrics_snapshot().fills >= 1);
}
