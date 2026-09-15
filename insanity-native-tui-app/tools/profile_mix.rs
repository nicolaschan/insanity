#[path = "../tests/common/unit_mixer.rs"]
mod unit_mixer;

use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::user_input_event::DenoiseSelection;
use unit_mixer::{push_chunk, render, unit_mixer};

fn main() {
    let denoise = std::env::args()
        .nth(1)
        .map(|arg| {
            if arg == "on" {
                DenoiseSelection::Nnnoiseless
            } else {
                DenoiseSelection::None
            }
        })
        .unwrap_or(DenoiseSelection::Nnnoiseless);
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(2000);
    let (mut mixer, _) = unit_mixer(100);
    let id1 = unit_mixer::add_unit_peer(&mut mixer, 100, denoise);
    let id2 = unit_mixer::add_unit_peer(&mut mixer, 100, denoise);
    let block1: Vec<f32> = (0..960)
        .map(|i| (440.0 * i as f32 / 48000.0 * std::f32::consts::TAU).sin() * 0.4)
        .collect();
    let block2: Vec<f32> = (0..960)
        .map(|i| (880.0 * i as f32 / 48000.0 * std::f32::consts::TAU).sin() * 0.4)
        .collect();
    let format = AudioFormat::new(2, 48000);
    let mut seq: u128 = 0;
    for _ in 0..3 {
        push_chunk(
            &mut mixer,
            id1,
            AudioChunk::new(seq, format.clone(), block1.clone()),
        );
        push_chunk(
            &mut mixer,
            id2,
            AudioChunk::new(seq, format.clone(), block2.clone()),
        );
        seq += 1;
    }
    let start = std::time::Instant::now();
    for _ in 0..iters {
        push_chunk(
            &mut mixer,
            id1,
            AudioChunk::new(seq, format.clone(), block1.clone()),
        );
        push_chunk(
            &mut mixer,
            id2,
            AudioChunk::new(seq, format.clone(), block2.clone()),
        );
        seq += 1;
        let out = render(&mut mixer, 960);
        std::hint::black_box(out);
    }
    let elapsed = start.elapsed();
    eprintln!(
        "profile_mix: {iters} iters in {elapsed:?} ({:.1}us/iter)",
        elapsed.as_micros() as f64 / iters as f64
    );
}
