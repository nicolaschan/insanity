use criterion::{Criterion, criterion_group, criterion_main};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::codec::AudioDecoder;
use insanity_core::audio::transform::ChunkTransform;
use insanity_native_tui_app::audio::codec::{OpusDecoder, OpusEncoder};
use std::hint::black_box;

fn bench_opus(c: &mut Criterion) {
    let mut group = c.benchmark_group("opus_e2e");
    let block: Vec<f32> = (0..960)
        .map(|i| (440.0 * i as f32 / 48000.0 * std::f32::consts::TAU).sin() * 0.4)
        .collect();
    let chunk = AudioChunk::new(0, AudioFormat::new(2, 48000), block);

    group.bench_function("encode_960_stereo", |b| {
        let mut encoder = OpusEncoder::new(48000, 2).expect("encoder");
        b.iter(|| {
            let frame = encoder.transform(chunk.clone()).expect("encode");
            black_box(frame);
        });
    });

    group.bench_function("encode_decode_roundtrip", |b| {
        let mut encoder = OpusEncoder::new(48000, 2).expect("encoder");
        let mut decoder = OpusDecoder::new(48000, 2).expect("decoder");
        b.iter(|| {
            let frame = encoder.transform(chunk.clone()).expect("encode");
            let out = decoder.decode(&frame).expect("decode");
            black_box(out);
        });
    });

    group.bench_function("encoder_new", |b| {
        b.iter(|| {
            black_box(OpusEncoder::new(48000, 2).expect("encoder"));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_opus);
criterion_main!(benches);
