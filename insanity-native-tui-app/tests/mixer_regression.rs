//! Mixer regression tests: behavior invariants kept permanently.
//!
//! Each test pins one hard-won behavior so future changes fail loudly:
//! - volume_max_contract (MAX_VOLUME=500 single source of truth)
//! - buffer_accounting_exact (JitterBuffer size accounting)
//! - seq_gap_is_time (mute/lagged seq-advance, no silence encode)
//! - plc_fades_not_holds (fade-to-zero instead of hold-last)
//! - mono_stereo_matrix (channel conversion)
//! - reconnect_resets (fresh subscription on reconnect)
//! - mixer_cleanup (unsubscribe on all exits)
//! - denoise_remainder_no_loss (chunks_exact tail)

use insanity_core::audio::denoiser::MultiChannelDenoiser;
use insanity_core::audio::jitter::JitterBuffer;
use insanity_core::audio::mixer::DEFAULT_JITTER_CHUNKS;
use insanity_core::audio::sample_ops::convert_to_mixer_channels;
use insanity_core::audio::transform::volume_multiplier;
use insanity_core::audio::{AudioFormat, chunk::AudioChunk};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio_test_support::{
    add_unit_peer, push_chunk, push_value, render, unit_mixer,
};
use insanity_native_tui_app::denoise::nnnoiseless::NnnoiselessDenoiser;
use insanity_native_tui_app::processor::MAX_VOLUME;

#[test]
fn jitter_target_pins_current_behavior() {
    assert_eq!(DEFAULT_JITTER_CHUNKS, 10);
    assert_eq!(MAX_VOLUME, 500);
}

#[test]
fn volume_max_contract_single_source_of_truth() {
    // All entry points must agree on MAX_VOLUME=500.
    assert_eq!(MAX_VOLUME, 500);
    assert_eq!(volume_multiplier(0), 0.0);
    assert!((volume_multiplier(100) - 1.0).abs() < 1e-6);
    // Pre-fix, 500 read equal to 200 (both clamped at min(200)); this fails on that behavior.
    assert!(
        volume_multiplier(500) > volume_multiplier(200),
        "500 {} should exceed 200 {} once single source of truth lands",
        volume_multiplier(500),
        volume_multiplier(200)
    );
    // Pre-fix, set_master_volume(999) pinned at 200; must now pin at 500.
    let (_, bus) = unit_mixer(100);
    bus.set(999);
    assert_eq!(
        bus.get(),
        MAX_VOLUME,
        "master volume must clamp at MAX_VOLUME"
    );
}

#[test]
fn buffer_accounting_exact_stale_drop() {
    // Stale chunk (seq < head after fast-forward) must not be returned.
    // Timing-preserving: slots 8,9 conceal (None) before 10 plays.
    let mut buf = JitterBuffer::new(3);
    buf.set(0, 0);
    buf.set(10, 10); // far-future jump: head fast-forwards past 0
    assert_eq!(buf.head(), 8, "head should fast-forward to 10-3+1");
    // 0 is stale and must never come out.
    assert_eq!(buf.next_item(), None); // conceal 8
    assert_eq!(buf.next_item(), None); // conceal 9
    assert_eq!(buf.next_item(), Some(10));
    assert_eq!(buf.next_item(), None);
    assert_eq!(buf.len(), 0);
}

#[test]
fn seq_gap_is_time() {
    // Seq 0,1 then 12 is a 100ms gap. Receiver must record it and must
    // NOT play chunk 12 immediately after chunk 1 (no time-compression).
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    // Single missing chunk (gap fits the 3-chunk window: span 0..2 < 3).
    push_value(&mut mixer, id, 0, 0.1);
    push_value(&mut mixer, id, 2, 0.9);
    let snap = mixer.metrics_snapshot();
    assert_eq!(
        snap.gap_detected, 1,
        "jump 0->2 must count one gap, got {snap:?}"
    );
    // First render drains seq 0. Second render conceals gap slot 1 (fade from
    // 0.1, NOT the 0.9 chunk). Third render plays seq 2 in its slot.
    let out = render(&mut mixer, 960);
    assert!((out[0] - 0.1).abs() < 1e-5, "first chunk {}", out[0]);
    let out2 = render(&mut mixer, 960);
    assert!(
        (out2[0] - 0.1).abs() < 1e-5,
        "gap concealment must fade from 0.1, got {} (chunk 2 played too early?)",
        out2[0]
    );
    assert!(
        out2[959].abs() < 0.05,
        "fade must reach ~0 by end of concealment chunk, got {}",
        out2[959]
    );
    let out3 = render(&mut mixer, 960);
    assert!(
        (out3[0] - 0.9).abs() < 1e-5,
        "seq 2 must play in its slot after 1 concealment, got {}",
        out3[0]
    );
}

#[test]
fn plc_fades_not_holds() {
    // Single lost chunk must decay to ~0 within one chunk, not hold DC.
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 0, 0.8);
    // Drain the real chunk.
    let out = render(&mut mixer, 960);
    assert!((out[0] - 0.8).abs() < 1e-5);
    // Underrun: must fade to silence, not hold 0.8 forever.
    let out2 = render(&mut mixer, 960);
    assert!(
        out2[959].abs() < 0.05,
        "PLC must fade to ~0 by end of concealment chunk, got {} (hold-last buzz)",
        out2[959]
    );
    let out3 = render(&mut mixer, 960);
    for s in out3.iter() {
        assert!(s.abs() < 0.05, "sustained loss must stay silent, got {s}");
        assert!(s.is_finite());
    }
    let snap = mixer.metrics_snapshot();
    assert!(snap.underrun > 0, "underruns must be counted, got {snap:?}");
}

#[test]
fn single_gap_counts_one_slot_and_full_fade_samples() {
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 0, 0.8);
    let _ = render(&mut mixer, 960);
    let _ = render(&mut mixer, 960);
    let snap = mixer.metrics_snapshot();
    assert_eq!(snap.underrun, 1, "one missed slot, got {snap:?}");
    assert_eq!(snap.plc_hold, 960, "one full fade run, got {snap:?}");
}

#[test]
fn plc_sample_to_slot_ratio_holds_across_callback_sizes() {
    for callback in [960usize, 2048, 4100] {
        let (mut mixer, _) = unit_mixer(100);
        let _ = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
        for _ in 0..4 {
            let _ = render(&mut mixer, callback);
        }
        let snap = mixer.metrics_snapshot();
        assert!(
            snap.underrun > 0,
            "callback {callback}: must starve, got {snap:?}"
        );
        let expected = snap.underrun as f64 * 960.0;
        let actual = snap.plc_hold as f64;
        assert!(
            (actual - expected).abs() <= callback as f64,
            "callback {callback}: plc {actual} must approx equal slots*960 {expected}, got {snap:?}"
        );
    }
}

#[test]
fn mono_stereo_matrix() {
    // Mono peer (480 samples) into stereo mixer must not misalign.
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    // Mono chunk: 480 samples of 0.5. Ingress duplicates to 960 stereo
    // samples, so one render is fully real (no PLC). Before the fix, the 480
    // mono samples plus 480 hold-last PLC accidentally looked the same on
    // the first render, so this asserts on underrun counters to distinguish.
    push_chunk(
        &mut mixer,
        id,
        AudioChunk::new(0, AudioFormat::new(1, 48000), vec![0.5; 480]),
    );
    let out = render(&mut mixer, 960);
    for (i, s) in out.iter().enumerate() {
        assert!(
            (*s - 0.5).abs() < 1e-5,
            "sample {i} should be duplicated mono 0.5, got {s}"
        );
    }
    let snap = mixer.metrics_snapshot();
    assert_eq!(
        snap.underrun, 0,
        "duplicated mono must serve 960 real samples with no PLC, got {snap:?}"
    );
}

#[test]
fn reconnect_resets_jitter() {
    // After head advances past 0, a fresh seq-0 stream (reconnect) must
    // not be entirely dropped as stale.
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    for seq in 0..10u128 {
        push_value(&mut mixer, id, seq, 0.3);
    }
    // Drain to advance head.
    let _ = render(&mut mixer, 960 * 10);
    // Simulate reconnect: fresh subscription (before the fix,
    // early-return kept stale head), then fresh seq 0 arrives.
    mixer.unsubscribe(id);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 0, 0.7);
    let out = render(&mut mixer, 960);
    assert!(
        (out[0] - 0.7).abs() < 1e-5,
        "reconnected stream must play, got {}",
        out[0]
    );
}

#[test]
fn mixer_cleanup_readd() {
    // remove_peer must be idempotent and allow clean re-add (documents the
    // select!-cancel leak path: stale PeerState must never survive).
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 0, 0.4);
    mixer.unsubscribe(id);
    mixer.unsubscribe(id); // idempotent
    assert_eq!(mixer.peer_count(), 0);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 0, 0.4);
    let out = render(&mut mixer, 960);
    assert!(out.iter().all(|s| (*s - 0.4).abs() < 1e-5));
}

#[test]
fn denoise_remainder_no_loss() {
    // chunks_exact must not drop tail samples.
    let mut denoiser: MultiChannelDenoiser<NnnoiselessDenoiser> = MultiChannelDenoiser::new();
    // 1000 stereo samples is not a multiple of 2*480=960.
    let data = vec![0.1f32; 1000];
    let chunk = AudioChunk::new(0, AudioFormat::new(2, 48000), data);
    let out = denoiser.denoise_chunk(&chunk);
    assert_eq!(out.sequence_number, 0);
    assert_eq!(
        out.audio_data.len(),
        1000,
        "denoiser must preserve length, dropped {} samples",
        1000 - out.audio_data.len()
    );
}

#[test]
fn metrics_counters_wired() {
    // Smoke test that metrics instrumentation is live without changing audio.
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 5, 0.1);
    let snap = mixer.metrics_snapshot();
    // First-ever chunk with nonzero seq counts as leading gap.
    assert_eq!(
        snap.gap_detected, 1,
        "leading gap must be counted, got {snap:?}"
    );
    // Normal in-order chunk: no extra gap.
    push_value(&mut mixer, id, 6, 0.1);
    assert_eq!(mixer.metrics_snapshot().gap_detected, 1);
    // Clipping counter: 0.6+0.6 tested elsewhere; here just check fills tick.
    let _ = render(&mut mixer, 10);
    assert!(mixer.metrics_snapshot().fills >= 1);
}

#[test]
fn reorder_hole_fill_does_not_count_gap() {
    let (mut mixer, _) = unit_mixer(100);
    let id = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    push_value(&mut mixer, id, 0, 0.1);
    push_value(&mut mixer, id, 2, 0.1);
    assert_eq!(mixer.metrics_snapshot().gap_detected, 1);
    push_value(&mut mixer, id, 1, 0.1);
    let snap = mixer.metrics_snapshot();
    assert_eq!(
        snap.gap_detected, 1,
        "hole fill must not recount, got {snap:?}"
    );
    assert_eq!(snap.late_dropped, 0, "hole fill is not late, got {snap:?}");
}

#[test]
fn channel_fallback_truncates_non_integral_tail() {
    let chunk = AudioChunk::new(0, AudioFormat::new(3, 48000), vec![0.5; 10]);
    let out = convert_to_mixer_channels(chunk, 2);
    assert_eq!(out.audio_data.len(), 6);
    let chunk = AudioChunk::new(0, AudioFormat::new(2, 48000), vec![0.5; 961]);
    let out = convert_to_mixer_channels(chunk, 1);
    assert_eq!(out.audio_data.len(), 480);
}
