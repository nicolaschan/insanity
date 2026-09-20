#[path = "common/audio_math.rs"]
mod audio_math;
#[path = "common/unit_mixer.rs"]
mod unit_mixer;

use audio_math::count_dips;
use insanity_core::user_input_event::DenoiseSelection;
use unit_mixer::{add_unit_peer, assert_all_finite, push_value, render, unit_mixer};

#[test]
fn empty_buffer_4096_matches_log_signature() {
    let (mut mixer, _) = unit_mixer(100);
    let _ = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    let fills = 10usize;
    let callback = 4096usize;
    for _ in 0..fills {
        let _ = render(&mut mixer, callback);
    }
    let snap = mixer.metrics_snapshot();
    assert_eq!(snap.gap_detected, 0, "no feed means no seq jumps: {snap:?}");
    assert_eq!(snap.late_dropped, 0, "{snap:?}");
    assert!(snap.underrun > 0, "empty buffer must underrun: {snap:?}");
    assert_eq!(
        snap.underrun, snap.fills,
        "one underrun event per concealment block: {snap:?}"
    );
    assert_eq!(
        snap.plc_hold,
        snap.fills * 960,
        "every synthesized sample concealed when fully starved: {snap:?}"
    );
}

#[test]
fn sustained_960_with_steady_feed_stays_clean() {
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    let mut seq: u128 = 0;
    for _ in 0..10 {
        push_value(&mut mixer, id, seq, 0.4);
        seq += 1;
    }
    for _ in 0..30 {
        let out = render(&mut mixer, 960);
        assert_all_finite(&out);
        push_value(&mut mixer, id, seq, 0.4);
        seq += 1;
    }
    let snap = mixer.metrics_snapshot();
    assert_eq!(snap.gap_detected, 0, "{snap:?}");
    assert_eq!(snap.underrun, 0, "{snap:?}");
    assert_eq!(snap.fills, 30);
}

#[test]
fn fully_starved_output_shows_repeated_dips() {
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 0, 0.5);
    let mut out = Vec::new();
    for _ in 0..20 {
        out.extend(render(&mut mixer, 960));
    }
    let dips = count_dips(&out, 960, 10.0);
    assert!(dips > 0, "repeated PLC fades must read as periodic dips");
}
