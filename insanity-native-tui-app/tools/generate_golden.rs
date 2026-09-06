use insanity_native_tui_app::audio::AudioMixer;
use insanity_native_tui_app::processor::{AudioChunk, AudioFormat};
use std::fs;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize},
};

fn write_f32_le(path: &Path, data: &[f32]) {
    let mut buf = Vec::with_capacity(data.len() * 4);
    for v in data {
        buf.extend(&v.to_le_bytes());
    }
    fs::write(path, buf).unwrap();
}

fn sine(freq: f32, sr: u32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| ((i as f32 * freq / sr as f32) * 2.0 * std::f32::consts::PI).sin() * 0.5)
        .collect()
}

fn main() {
    let out = Path::new("insanity-native-tui-app/testdata/golden");
    fs::create_dir_all(out).unwrap();

    // sine 440
    let s440: Vec<f32> = sine(440.0, 48000, 480 * 20); // 20 chunks
    // duplicate for stereo interleaved: just repeat same value for L/R
    let mut stereo = Vec::new();
    for v in s440 {
        stereo.push(v);
        stereo.push(v);
    }
    write_f32_le(&out.join("sine_440.raw"), &stereo);
    println!("wrote sine_440 {}", stereo.len());

    // sweep 100-8000
    let mut sweep = Vec::new();
    for i in 0..480 * 20 {
        let t = i as f32 / 48000.0;
        let freq = 100.0 + (7900.0 * t / (480.0 * 20.0 / 48000.0));
        let v = (t * freq * 2.0 * std::f32::consts::PI).sin() * 0.5;
        sweep.push(v);
        sweep.push(v);
    }
    write_f32_le(&out.join("sweep.raw"), &sweep);
    println!("wrote sweep {}", sweep.len());

    // two peer mix via AudioMixer
    let mixer = AudioMixer::new_no_device();
    let v1 = Arc::new(AtomicUsize::new(100));
    let d1 = Arc::new(AtomicBool::new(false));
    let v2 = Arc::new(AtomicUsize::new(100));
    let d2 = Arc::new(AtomicBool::new(false));
    let id1 = uuid::Uuid::new_v4();
    let id2 = uuid::Uuid::new_v4();
    mixer.add_peer(id1, v1, d1, None);
    mixer.add_peer(id2, v2, d2, None);
    // Interleaved feed/fill per chunk: works with any jitter window >= 1
    // (bulk feed-then-fill would evict under small windows).
    let mut mixed = Vec::with_capacity(960 * 10);
    for seq in 0..10 {
        let chunk: Vec<f32> = sine(440.0, 48000, 960);
        mixer.handle_incoming(
            id1,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), chunk.clone()),
        );
        let chunk2: Vec<f32> = sine(880.0, 48000, 960);
        mixer.handle_incoming(
            id2,
            AudioChunk::new(seq, AudioFormat::new(2, 48000), chunk2),
        );
        let mut out = vec![0f32; 960];
        mixer.fill_buffer(&mut out);
        mixed.extend_from_slice(&out);
    }
    write_f32_le(&out.join("two_peer_mix.raw"), &mixed);
    println!("wrote two_peer_mix {}", mixed.len());
}
