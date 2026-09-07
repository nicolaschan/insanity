use insanity_core::audio::chunk::AudioChunk;
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::AudioMixer;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::{Arc, atomic::AtomicUsize};

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

    // two peer mix via AudioMixer.
    let mixer = AudioMixer::new_no_device();
    let v1 = Arc::new(AtomicUsize::new(100));
    let d1 = Arc::new(Mutex::new(DenoiseSelection::None));
    let v2 = Arc::new(AtomicUsize::new(100));
    let d2 = Arc::new(Mutex::new(DenoiseSelection::None));
    let id1 = uuid::Uuid::new_v4();
    let id2 = uuid::Uuid::new_v4();
    mixer.add_peer(id1, v1, d1, None);
    mixer.add_peer(id2, v2, d2, None);
    // Interleaved feed/fill per chunk: works with any jitter window >= 1
    // (bulk feed-then-fill would evict under small windows).
    let mut mixed = Vec::with_capacity(960 * 10);
    for seq in 0..10 {
        let chunk: Vec<f32> = sine(440.0, 48000, 960);
        mixer.handle_incoming(id1, AudioChunk::new(seq, chunk.clone()), 2);
        let chunk2: Vec<f32> = sine(880.0, 48000, 960);
        mixer.handle_incoming(id2, AudioChunk::new(seq, chunk2), 2);
        let mut out = vec![0f32; 960];
        mixer.fill_buffer(&mut out);
        mixed.extend_from_slice(&out);
    }
    write_f32_le(&out.join("two_peer_mix.raw"), &mixed);
    println!("wrote two_peer_mix {}", mixed.len());
}
