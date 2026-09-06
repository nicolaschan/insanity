use insanity_native_tui_app::audio::AudioMixer;
use insanity_native_tui_app::audio_test_support::energy_ratio;
use insanity_native_tui_app::processor::{AudioChunk, AudioFormat, MultiChannelDenoiser};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize},
};

fn music_chunk() -> Vec<f32> {
    (0..480)
        .flat_map(|i| {
            let t = i as f32 / 48000.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.35
                + (2.0 * std::f32::consts::PI * 660.0 * t).sin() * 0.35;
            vec![s, s]
        })
        .collect()
}

fn noise_chunk(seed: u64, amp: f32) -> Vec<f32> {
    let mut state = seed;
    (0..960)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let u = ((state >> 16) & 0xFFFF) as f32 / 65535.0;
            (u * 2.0 - 1.0) * amp
        })
        .collect()
}

fn mixer_with_denoise(denoise: bool) -> (AudioMixer, uuid::Uuid) {
    let mixer = AudioMixer::new_no_device();
    let id = uuid::Uuid::new_v4();
    mixer.add_peer(
        id,
        Arc::new(AtomicUsize::new(100)),
        Arc::new(AtomicBool::new(denoise)),
        None,
    );
    (mixer, id)
}

#[test]
fn music_intact_when_denoise_off() {
    let (mixer, id) = mixer_with_denoise(false);
    let music = music_chunk();
    assert!(music.iter().all(|s| s.abs() < 1.0));
    mixer.handle_incoming(
        id,
        AudioChunk::new(0, AudioFormat::new(2, 48000), music.clone()),
    );
    let mut out = vec![0f32; 960];
    mixer.fill_buffer(&mut out);
    assert_eq!(out.len(), music.len());
    for (i, (a, b)) in out.iter().zip(music.iter()).enumerate() {
        assert_eq!(a, b, "sample {i} altered with denoise off");
    }
}

#[test]
fn noise_substantially_quieter_when_denoise_on() {
    let mut worst_ratio = 0.0f64;
    for &amp in &[0.1f32, 0.25, 0.4] {
        for run in 0..5u64 {
            let seed = (run + 1).wrapping_mul(0x9E3779B97F4A7C15);
            let mut denoiser = MultiChannelDenoiser::new();
            let mut ins = Vec::new();
            let mut outs = Vec::new();
            for seq in 0..6u128 {
                let chunk = noise_chunk(seed ^ (seq as u64 + 1), amp);
                let denoised = denoiser.denoise_chunk(&AudioChunk::new(
                    seq,
                    AudioFormat::new(2, 48000),
                    chunk.clone(),
                ));
                assert_eq!(denoised.sequence_number, seq);
                assert_eq!(denoised.audio_data.len(), 960);
                if seq >= 3 {
                    ins.extend_from_slice(&chunk);
                    outs.extend_from_slice(&denoised.audio_data);
                }
            }
            let ratio = energy_ratio(&outs, &ins);
            worst_ratio = worst_ratio.max(ratio);
            eprintln!("amp={amp} run={run} tail energy ratio={ratio:.4}");
        }
    }
    eprintln!("worst tail energy ratio={worst_ratio:.4}");
    assert!(
        worst_ratio <= 0.34,
        "denoiser must substantially suppress non-speech, worst ratio {worst_ratio:.4}"
    );
}

#[test]
fn toggle_honored_on_nonspeech() {
    let music = noise_chunk(0x12345678, 0.4);
    let (mixer_off, id_off) = mixer_with_denoise(false);
    mixer_off.handle_incoming(
        id_off,
        AudioChunk::new(0, AudioFormat::new(2, 48000), music.clone()),
    );
    let mut out_off = vec![0f32; 960];
    mixer_off.fill_buffer(&mut out_off);
    let (mixer_on, id_on) = mixer_with_denoise(true);
    for seq in 0..6u128 {
        mixer_on.handle_incoming(
            id_on,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), music.clone()),
        );
    }
    for _ in 0..5 {
        let mut discard = vec![0f32; 960];
        mixer_on.fill_buffer(&mut discard);
    }
    let mut out_on = vec![0f32; 960];
    mixer_on.fill_buffer(&mut out_on);
    let ratio = energy_ratio(&out_on, &out_off);
    assert!(
        ratio < 0.8,
        "denoise on/off must route differently on non-speech, ratio {ratio:.4}"
    );
}
