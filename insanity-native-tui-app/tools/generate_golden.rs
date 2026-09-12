#[path = "../tests/common/audio_math.rs"]
mod audio_math;
#[path = "../tests/common/unit_mixer.rs"]
mod unit_mixer;

use audio_math::sine;
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::user_input_event::DenoiseSelection;
use std::fs;
use std::path::Path;
use unit_mixer::{add_unit_peer, push_chunk, render, unit_mixer};

fn write_f32_le(path: &Path, data: &[f32]) {
    let mut buf = Vec::with_capacity(data.len() * 4);
    for v in data {
        buf.extend(&v.to_le_bytes());
    }
    fs::write(path, buf).unwrap();
}

fn main() {
    let out = Path::new("insanity-native-tui-app/testdata/golden");
    fs::create_dir_all(out).unwrap();

    // two peer mix via unit mixer.
    let (mut mixer, _) = unit_mixer(100);
    let id1 = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    let id2 = add_unit_peer(&mut mixer, 100, DenoiseSelection::None);
    // Interleaved feed/fill per chunk: works with any jitter window >= 1
    // (bulk feed-then-fill would evict under small windows).
    let mut mixed = Vec::with_capacity(960 * 10);
    for seq in 0..10 {
        let chunk: Vec<f32> = sine(440.0, 48000, 960);
        push_chunk(
            &mut mixer,
            id1,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), chunk.clone()),
        );
        let chunk2: Vec<f32> = sine(880.0, 48000, 960);
        push_chunk(
            &mut mixer,
            id2,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), chunk2),
        );
        mixed.extend_from_slice(&render(&mut mixer, 960));
    }
    write_f32_le(&out.join("two_peer_mix.raw"), &mixed);
    println!("wrote two_peer_mix {}", mixed.len());
}
