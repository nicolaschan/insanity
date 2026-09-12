#![allow(dead_code)]
use insanity_core::loudness::calculate_loudness;

pub fn loudness(samples: &[f32]) -> f64 {
    calculate_loudness(samples)
}

pub fn energy_ratio(actual: &[f32], reference: &[f32]) -> f64 {
    assert_eq!(actual.len(), reference.len());
    let e_ref: f64 = reference.iter().map(|v| (*v as f64).powi(2)).sum();
    let e_act: f64 = actual.iter().map(|v| (*v as f64).powi(2)).sum();
    e_act / e_ref.max(1e-12)
}

fn xcorr_at(a: &[f32], r: &[f32], shift: usize) -> f64 {
    let (dot, ea, er) =
        a.iter()
            .skip(shift)
            .zip(r.iter())
            .fold((0.0, 0.0, 0.0), |(dot, ea, er), (x, y)| {
                let (x, y) = (*x as f64, *y as f64);
                (dot + x * y, ea + x * x, er + y * y)
            });
    dot / (ea * er).sqrt().max(1e-12)
}

pub fn max_normalized_xcorr(actual: &[f32], reference: &[f32], max_lag: usize) -> f64 {
    assert_eq!(actual.len(), reference.len());
    let lag = max_lag.min(actual.len().saturating_sub(1));
    (0..=lag)
        .flat_map(|shift| [(actual, reference, shift), (reference, actual, shift)])
        .map(|(a, r, shift)| xcorr_at(a, r, shift))
        .fold(0.0, f64::max)
}

pub fn sine(freq: f32, sr: u32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| ((i as f32 * freq / sr as f32) * 2.0 * std::f32::consts::PI).sin() * 0.5)
        .collect()
}

pub fn snr(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sig: f64 = a.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / a.len() as f64;
    let err: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| ((*x - *y) as f64).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    10.0 * (sig / err.max(1e-12)).log10()
}

pub fn window_rms(samples: &[f32]) -> f64 {
    let e: f64 = samples.iter().map(|v| (*v as f64).powi(2)).sum();
    (e / samples.len().max(1) as f64).sqrt()
}

pub fn count_dips(samples: &[f32], window: usize, thresh_db: f64) -> usize {
    let mut energies: Vec<f64> = samples
        .chunks(window)
        .filter(|w| w.len() == window)
        .map(window_rms)
        .collect();
    energies.sort_by(|a, b| a.partial_cmp(b).expect("rms"));
    let median = energies[energies.len() / 2];
    energies
        .iter()
        .filter(|e| 20.0 * (**e / median.max(1e-9)).log10() < -thresh_db)
        .count()
}

pub fn tail(samples: &[f32], chunks: usize) -> &[f32] {
    &samples[samples.len() - chunks * 960..]
}
