use criterion::{Criterion, criterion_group, criterion_main};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::sample::Resampler;
use insanity_core::audio::transform::{ChunkTransform, Gain};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::mixer as app_mixer;
use insanity_native_tui_app::audio::mixer::{MAX_VOLUME, PeerControls, chain_from_controls};
use rubato_audio_source::StreamResampler;
use std::hint::black_box;

#[path = "../tests/common/unit_mixer.rs"]
mod unit_mixer;

use unit_mixer::{push_chunk, render, unit_mixer};

fn sine_block(freq: f32) -> Vec<f32> {
    (0..960)
        .map(|i| (freq * i as f32 / 48000.0 * std::f32::consts::TAU).sin() * 0.4)
        .collect()
}

fn bench_mixer(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixer_2peer");
    for denoise in [DenoiseSelection::None, DenoiseSelection::Nnnoiseless] {
        let name = match denoise {
            DenoiseSelection::None => "denoise_off",
            DenoiseSelection::Nnnoiseless => "denoise_on",
        };
        group.bench_function(name, |b| {
            let (mut mixer, _) = unit_mixer(100);
            let id1 = unit_mixer::add_unit_peer(&mut mixer, 100, denoise);
            let id2 = unit_mixer::add_unit_peer(&mut mixer, 100, denoise);
            let block1 = sine_block(440.0);
            let block2 = sine_block(880.0);
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
            assert_eq!(
                mixer.metrics_snapshot().overflow_dropped,
                0,
                "bench setup overflowed the jitter buffer"
            );
            b.iter(|| {
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
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_mixer_idle(c: &mut Criterion) {
    c.bench_function("mixer_0peer_idle", |b| {
        let (mut mixer, _) = unit_mixer(100);
        b.iter(|| {
            let out = render(&mut mixer, 960);
            black_box(out);
        });
    });
}

fn bench_real_chain_construction(c: &mut Criterion) {
    c.bench_function("peerchain_from_controls", |b| {
        b.iter(|| {
            let controls = PeerControls::new(100, DenoiseSelection::Nnnoiseless);
            black_box(chain_from_controls(&controls));
        });
    });
}

fn bench_gain_shared(c: &mut Criterion) {
    c.bench_function("gain_shared", |b| {
        b.iter(|| {
            let (_, control) = Gain::shared(100, MAX_VOLUME);
            black_box(control);
        });
    });
}

fn bench_output_resampler_new(c: &mut Criterion) {
    let config = AudioPipelineConfig::default();
    c.bench_function("output_resampler_new", |b| {
        b.iter(|| {
            black_box(app_mixer::output_resampler(
                AudioFormat::new(2, config.sample_rate()),
                config,
            ));
        });
    });
    c.bench_function("output_resampler_new_48k_to_44100", |b| {
        b.iter(|| {
            black_box(app_mixer::output_resampler(
                AudioFormat::new(2, 44100),
                config,
            ));
        });
    });
}

fn bench_stream_resampler_push_pop(c: &mut Criterion) {
    let config = AudioPipelineConfig::default();
    c.bench_function("resampler_push_pop_960", |b| {
        let mut resampler = StreamResampler::new(
            AudioFormat::new(2, config.sample_rate()),
            config.sample_rate(),
            config.frames(),
        );
        let input = sine_block(440.0);
        b.iter(|| {
            for sample in &input {
                resampler.push_sample(*sample);
            }
            let mut count = 0;
            while resampler.pop_sample().is_some() {
                count += 1;
            }
            black_box(count);
        });
    });
    c.bench_function("resampler_push_pop_960_44100_to_48000", |b| {
        let mut resampler = StreamResampler::new(
            AudioFormat::new(2, 44100),
            config.sample_rate(),
            config.frames(),
        );
        let input = sine_block(440.0);
        b.iter(|| {
            for sample in &input {
                resampler.push_sample(*sample);
            }
            let mut count = 0;
            while resampler.pop_sample().is_some() {
                count += 1;
            }
            black_box(count);
        });
    });
}

fn bench_peerchain_transform(c: &mut Criterion) {
    let mut group = c.benchmark_group("peerchain_transform");
    for (denoise, volume) in [
        (DenoiseSelection::None, 100),
        (DenoiseSelection::Nnnoiseless, 100),
        (DenoiseSelection::Nnnoiseless, 150),
    ] {
        let name = match (denoise, volume) {
            (DenoiseSelection::None, _) => "denoise_off_vol100",
            (DenoiseSelection::Nnnoiseless, 100) => "denoise_on_vol100",
            (DenoiseSelection::Nnnoiseless, _) => "denoise_on_vol150",
        };
        group.bench_function(name, |b| {
            let controls = PeerControls::new(volume, denoise);
            let mut chain = chain_from_controls(&controls);
            let chunk = AudioChunk::new(0, AudioFormat::new(2, 48000), sine_block(440.0));
            b.iter(|| {
                black_box(chain.transform(chunk.clone()));
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_mixer,
    bench_mixer_idle,
    bench_real_chain_construction,
    bench_gain_shared,
    bench_output_resampler_new,
    bench_stream_resampler_push_pop,
    bench_peerchain_transform
);
criterion_main!(benches);
