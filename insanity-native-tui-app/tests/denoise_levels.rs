use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::denoiser::MultiChannelDenoiser;
use insanity_native_tui_app::audio::denoise::NnnoiselessDenoiser;
use std::path::Path;

fn fixture_path() -> &'static Path {
    if Path::new("insanity-native-tui-app/tests/testdata/speech_sample.raw").exists() {
        Path::new("insanity-native-tui-app/tests/testdata/speech_sample.raw")
    } else {
        Path::new("tests/testdata/speech_sample.raw")
    }
}

fn read_f32_le(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn rms(samples: &[f32]) -> f64 {
    let e: f64 = samples.iter().map(|v| (*v as f64).powi(2)).sum();
    (e / samples.len().max(1) as f64).sqrt()
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.max(1e-12).log10()
}

fn speech() -> Vec<f32> {
    let samples = read_f32_le(fixture_path());
    assert_eq!(samples.len(), 48000 * 3 * 2, "3s stereo 48kHz fixture");
    samples
}

fn steady_samples() -> (Vec<[f32; 960]>, Vec<f32>) {
    let samples = speech();
    let (chunks, _) = samples.as_chunks::<960>();
    let steady: Vec<f32> = chunks.iter().skip(3).flatten().copied().collect();
    (chunks.to_vec(), steady)
}

#[test]
fn speech_denoiser_preserves_level() {
    let fmt = AudioFormat::new(2, 48000);
    let (chunks, steady) = steady_samples();
    let mut denoiser: MultiChannelDenoiser<NnnoiselessDenoiser> = MultiChannelDenoiser::new();
    let mut outs = Vec::new();
    for (seq, chunk) in chunks.iter().enumerate() {
        let out = denoiser.denoise_chunk(AudioChunk::new(seq as u128, fmt.clone(), chunk.to_vec()));
        if seq >= 3 {
            outs.extend_from_slice(&out.audio_data);
        }
    }
    let ratio = rms(&outs) / rms(&steady);
    eprintln!("speech denoise steady-state db={:.2}", db(ratio));
    assert!(
        (0.88..1.05).contains(&ratio),
        "denoiser changed speech level: ratio {ratio:.4} (db {:.2}), expected ~1.0",
        db(ratio)
    );
}
