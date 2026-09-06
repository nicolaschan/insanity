use insanity_native_tui_app::audio::{
    AudioMixer, JITTER_TARGET_CHUNKS, buffer_starved, format_metrics_line,
};
use insanity_native_tui_app::processor::{AudioChunk, AudioFormat};
use opus::{Application, Channels, Decoder, Encoder};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize},
};

fn mixer_with_capacity(chunks: usize) -> AudioMixer {
    AudioMixer::new_no_device_with_format_and_capacity(48000, 2, chunks)
}

fn add_peer(mixer: &AudioMixer) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    mixer.add_peer(
        id,
        Arc::new(AtomicUsize::new(100)),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    id
}

fn feed(mixer: &AudioMixer, id: uuid::Uuid, first_seq: u128, count: usize, value: f32) {
    for seq in first_seq..first_seq + count as u128 {
        mixer.handle_incoming(
            id,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), vec![value; 960]),
        );
    }
}

fn fill(mixer: &AudioMixer, samples: usize) -> Vec<f32> {
    let mut out = vec![0f32; samples];
    mixer.fill_buffer(&mut out);
    out
}

fn underruns(mixer: &AudioMixer) -> usize {
    mixer.metrics_snapshot().underrun
}

fn plc_hold(mixer: &AudioMixer) -> usize {
    mixer.metrics_snapshot().plc_hold
}

struct Cell {
    capacity: usize,
    callback: usize,
    fills: usize,
    feed_pattern: [usize; 4],
    expect_starved: bool,
    expect_every_fill: bool,
}

#[test]
fn callback_demand_vs_buffer_capacity() {
    let cells = vec![
        Cell {
            capacity: 3,
            callback: 4100,
            fills: 25,
            feed_pattern: [4, 4, 4, 5],
            expect_starved: true,
            expect_every_fill: true,
        },
        Cell {
            capacity: 5,
            callback: 4100,
            fills: 25,
            feed_pattern: [5, 4, 4, 4],
            expect_starved: false,
            expect_every_fill: false,
        },
        Cell {
            capacity: 10,
            callback: 4100,
            fills: 25,
            feed_pattern: [5, 4, 4, 4],
            expect_starved: false,
            expect_every_fill: false,
        },
        Cell {
            capacity: 3,
            callback: 2048,
            fills: 35,
            feed_pattern: [2, 2, 2, 2],
            expect_starved: true,
            expect_every_fill: false,
        },
        Cell {
            capacity: 10,
            callback: 2048,
            fills: 35,
            feed_pattern: [2, 2, 2, 2],
            expect_starved: false,
            expect_every_fill: false,
        },
        Cell {
            capacity: 3,
            callback: 960,
            fills: 25,
            feed_pattern: [1, 1, 1, 1],
            expect_starved: false,
            expect_every_fill: false,
        },
        Cell {
            capacity: 10,
            callback: 960,
            fills: 25,
            feed_pattern: [1, 1, 1, 1],
            expect_starved: false,
            expect_every_fill: false,
        },
    ];
    for cell in cells {
        let mixer = mixer_with_capacity(cell.capacity);
        let id = add_peer(&mixer);
        feed(&mixer, id, 0, cell.capacity, 0.4);
        let mut next_seq = cell.capacity as u128;
        let mut starved_fills = 0usize;
        for f in 0..cell.fills {
            let before = underruns(&mixer);
            let out = fill(&mixer, cell.callback);
            for s in out.iter() {
                assert!(s.is_finite());
            }
            if underruns(&mixer) > before {
                starved_fills += 1;
            }
            let n = cell.feed_pattern[f % cell.feed_pattern.len()];
            feed(&mixer, id, next_seq, n, 0.4);
            next_seq += n as u128;
        }
        let snap = mixer.metrics_snapshot();
        let label = format!("cap={} callback={}", cell.capacity, cell.callback);
        assert_eq!(
            snap.gap_detected, 0,
            "{label}: ingress healthy, got {snap:?}"
        );
        assert_eq!(
            snap.late_dropped, 0,
            "{label}: ingress healthy, got {snap:?}"
        );
        if cell.expect_starved {
            assert!(
                snap.underrun > 0,
                "{label}: oversized callback must starve a small buffer, got {snap:?}"
            );
            let occ = mixer.peer_occupancy(&id).unwrap_or(usize::MAX);
            assert!(
                occ > 0,
                "{label}: buffer still holds audio yet the callback starves mid-fill, occupancy {occ}"
            );
        } else {
            assert_eq!(
                snap.underrun, 0,
                "{label}: adequate capacity must not starve, got {snap:?}"
            );
        }
        if cell.expect_every_fill {
            assert_eq!(
                starved_fills, cell.fills,
                "{label}: incident shape starves every fill, got {starved_fills}/{}",
                cell.fills
            );
        }
    }
}

#[test]
fn sustained_underfeed_starves_any_finite_buffer() {
    let mixer = mixer_with_capacity(10);
    let id = add_peer(&mixer);
    feed(&mixer, id, 0, 10, 0.4);
    let mut next_seq = 10u128;
    let mut first_starved: Option<usize> = None;
    for f in 0..30 {
        let before = underruns(&mixer);
        let occ_before = mixer.peer_occupancy(&id).unwrap_or(0);
        fill(&mixer, 4100);
        let delta = underruns(&mixer) - before;
        if delta > 0 && first_starved.is_none() {
            first_starved = Some(f);
            assert!(
                !buffer_starved(delta, &[(id.to_string(), occ_before)], 10),
                "underfeed onset must classify as supply-side, not structural"
            );
        }
        feed(&mixer, id, next_seq, 4, 0.4);
        next_seq += 4;
    }
    let onset = first_starved.expect("sustained underfeed must eventually starve");
    assert!(
        (18..=26).contains(&onset),
        "onset near capacity/drain-rate horizon, got fill {onset}"
    );
}

#[test]
fn playback_before_data_conceals_then_recovers() {
    let mixer = mixer_with_capacity(10);
    let id = add_peer(&mixer);
    let out = fill(&mixer, 4100);
    assert_eq!(
        plc_hold(&mixer),
        4100,
        "empty buffer conceals the whole fill"
    );
    let slots = underruns(&mixer);
    assert!(
        slots > 0 && slots < 4100,
        "slots {slots} must be fewer than samples"
    );
    assert!(out.iter().all(|s| s.abs() < 1e-6));
    feed(&mixer, id, 0, 10, 0.4);
    fill(&mixer, 4100);
    let before = underruns(&mixer);
    let out = fill(&mixer, 4100);
    assert_eq!(underruns(&mixer), before, "primed buffer must serve clean");
    assert!((out[0] - 0.4).abs() < 1e-5);
}

#[test]
fn starved_predicate_matches_incident_and_healthy_logs() {
    let peer = "36f72d6a-60d5-9cd1-2b3b-e9a706b80fec".to_string();
    assert!(buffer_starved(291264, &[(peer.clone(), 3)], 3));
    assert!(buffer_starved(261888, &[(peer.clone(), 3)], 3));
    assert!(!buffer_starved(0, &[(peer.clone(), 5)], 10));
    assert!(!buffer_starved(5760, &[(peer.clone(), 1)], 10));
    assert!(!buffer_starved(100, &[], 3));
    assert!(!buffer_starved(0, &[(peer.clone(), 3)], 3));
    let line = format_metrics_line(
        &insanity_native_tui_app::audio::MixerMetricsSnapshot::default(),
        &insanity_native_tui_app::audio::MixerMetricsSnapshot {
            gap_detected: 0,
            late_dropped: 0,
            underrun: 291264,
            plc_hold: 291264,
            clip_hits: 495,
            fills: 234,
        },
        256299,
        &[(peer, 3)],
    );
    assert!(line.contains("underruns=291264"), "{line}");
    assert!(buffer_starved(
        291264,
        &[("36f72d6a-60d5-9cd1-2b3b-e9a706b80fec".to_string(), 3)],
        3
    ));
}

#[test]
fn recovery_after_jump_costs_capacity_minus_one_fills() {
    for (capacity, expected_fills) in [(10usize, 9usize), (30usize, 29usize)] {
        let mixer = mixer_with_capacity(capacity);
        let id = add_peer(&mixer);
        feed(&mixer, id, 0, capacity, 0.1);
        let gaps_before = mixer.metrics_snapshot().gap_detected;
        feed(&mixer, id, 100, 1, 0.9);
        assert_eq!(
            mixer.metrics_snapshot().gap_detected,
            gaps_before + 1,
            "jump must be recorded"
        );
        assert_eq!(mixer.peer_occupancy(&id), Some(1));
        let mut recovery_fills = 0usize;
        loop {
            let out = fill(&mixer, 960);
            if (out[0] - 0.9).abs() < 1e-5 {
                break;
            }
            recovery_fills += 1;
            assert!(
                recovery_fills <= capacity,
                "cap {capacity}: recovery took too long"
            );
        }
        assert_eq!(
            recovery_fills, expected_fills,
            "cap {capacity}: far jump costs capacity-1 concealment fills"
        );
    }
}

#[test]
fn production_capacity_recovers_within_ten_fills() {
    let mixer = AudioMixer::new_no_device();
    let id = add_peer(&mixer);
    feed(&mixer, id, 0, 10, 0.1);
    feed(&mixer, id, 100, 1, 0.9);
    let mut recovery_fills = 0usize;
    loop {
        let out = fill(&mixer, 960);
        if (out[0] - 0.9).abs() < 1e-5 {
            break;
        }
        recovery_fills += 1;
        assert!(recovery_fills <= 10, "production buffer must recover fast");
    }
}

#[test]
fn summed_peers_count_clips_and_stay_bounded() {
    let mixer = mixer_with_capacity(10);
    let ids: Vec<uuid::Uuid> = (0..2)
        .map(|_| {
            let id = uuid::Uuid::new_v4();
            mixer.add_peer(
                id,
                Arc::new(AtomicUsize::new(100)),
                Arc::new(AtomicBool::new(false)),
                None,
            );
            id
        })
        .collect();
    for id in &ids {
        feed(&mixer, *id, 0, 3, 0.6);
    }
    let out = fill(&mixer, 960);
    assert!((out[0] - 1.0).abs() < 1e-5, "0.6+0.6 clamps to 1.0");
    assert!(
        mixer.metrics_snapshot().clip_hits > 0,
        "clamping must be counted"
    );
    for s in out.iter() {
        assert!(s.is_finite());
        assert!(s.abs() <= 1.0 + 1e-6);
    }
}

#[test]
fn hot_opus_single_peer_clip_rate_below_five_percent() {
    let mut enc = Encoder::new(48000, Channels::Stereo, Application::Audio).expect("encoder");
    let mut dec = Decoder::new(48000, Channels::Stereo).expect("decoder");
    let mut hot = vec![0f32; 960];
    for _ in 0..6 {
        let frame: Vec<f32> = (0..480)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 / 48000.0 * 2.0 * std::f32::consts::PI).sin() * 0.99;
                vec![s, s]
            })
            .collect();
        let payload = enc.encode_vec_float(&frame, 65535).expect("encode");
        let nb = dec.get_nb_samples(&payload).expect("nb");
        hot = vec![0f32; nb * 2];
        dec.decode_float(&payload, &mut hot, false).expect("decode");
    }
    assert_eq!(hot.len(), 960);
    let mixer = mixer_with_capacity(10);
    let id = add_peer(&mixer);
    for seq in 0..10u128 {
        mixer.handle_incoming(
            id,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), hot.clone()),
        );
    }
    let mut total = 0usize;
    for _ in 0..10 {
        total += fill(&mixer, 960).len();
    }
    let clips = mixer.metrics_snapshot().clip_hits;
    let rate = clips as f64 / total.max(1) as f64;
    assert!(
        rate < 0.05,
        "hot opus clip rate {rate:.4} ({clips}/{total}) exceeds 5%"
    );
}

#[test]
fn producer_stall_conceals_without_gap_and_predicate_stays_quiet() {
    let mixer = mixer_with_capacity(10);
    let id = add_peer(&mixer);
    feed(&mixer, id, 0, 10, 0.4);
    let mut next_seq = 10u128;
    let mut stall_observation: Option<(usize, usize)> = None;
    for f in 0..12 {
        let stalled = (5..8).contains(&f);
        let before = underruns(&mixer);
        let occ_before = mixer.peer_occupancy(&id).unwrap_or(0);
        fill(&mixer, 4100);
        let delta = underruns(&mixer) - before;
        if stalled && delta > 0 && stall_observation.is_none() {
            stall_observation = Some((delta, occ_before));
        }
        if !stalled {
            feed(&mixer, id, next_seq, 4, 0.4);
            next_seq += 4;
        }
    }
    let snap = mixer.metrics_snapshot();
    assert!(snap.underrun > 0, "stall must conceal, got {snap:?}");
    assert_eq!(
        snap.gap_detected, 0,
        "sequential resume records no gap, got {snap:?}"
    );
    assert_eq!(
        snap.late_dropped, 0,
        "sequential resume records no late, got {snap:?}"
    );
    let (delta, occ) = stall_observation.expect("stall must underrun");
    assert!(
        !buffer_starved(delta, &[(id.to_string(), occ)], 10),
        "low-occupancy stall must not classify as structural"
    );
}

#[test]
fn bulk_overfeed_silently_drops_early_audio_with_clean_counters() {
    let mixer = mixer_with_capacity(10);
    let id = add_peer(&mixer);
    for seq in 0..20u128 {
        mixer.handle_incoming(
            id,
            AudioChunk::new(
                seq,
                AudioFormat::new(2, 48000),
                vec![seq as f32 * 0.01; 960],
            ),
        );
    }
    assert_eq!(mixer.peer_occupancy(&id), Some(10));
    let mut played = Vec::new();
    for _ in 0..9 {
        let out = fill(&mixer, 960);
        played.push((out[0] / 0.01).round() as u128);
    }
    assert_eq!(played.len(), 9);
    for w in played.windows(2) {
        assert_eq!(
            w[1],
            w[0] + 1,
            "played run must be contiguous, got {played:?}"
        );
    }
    assert!(
        played[0] >= 10,
        "bulk overfeed must fast-forward past early audio, first played {}",
        played[0]
    );
    let snap = mixer.metrics_snapshot();
    assert_eq!(snap.underrun, 0, "no starvation, got {snap:?}");
    assert_eq!(
        snap.gap_detected, 0,
        "evicted audio records no gap, got {snap:?}"
    );
    assert_eq!(
        snap.late_dropped, 0,
        "evicted audio records no late, got {snap:?}"
    );
}

#[test]
fn production_capacity_covers_observed_callback() {
    let mixer = AudioMixer::new_no_device();
    let id = add_peer(&mixer);
    feed(&mixer, id, 0, JITTER_TARGET_CHUNKS, 0.4);
    let mut next_seq = JITTER_TARGET_CHUNKS as u128;
    let pattern = [4usize, 4, 4, 5];
    for f in 0..25 {
        let out = fill(&mixer, 4100);
        for s in out.iter() {
            assert!(s.is_finite());
        }
        let n = pattern[f % pattern.len()];
        feed(&mixer, id, next_seq, n, 0.4);
        next_seq += n as u128;
    }
    let snap = mixer.metrics_snapshot();
    assert_eq!(
        snap.underrun, 0,
        "production buffer must cover a 4100-sample callback, got {snap:?}"
    );
    assert_eq!(
        snap.gap_detected, 0,
        "matched producer records no gap, got {snap:?}"
    );
    assert_eq!(
        snap.late_dropped, 0,
        "matched producer records no late, got {snap:?}"
    );
}
