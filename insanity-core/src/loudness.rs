pub fn calculate_loudness(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }

    // Calculate the root mean square (RMS)
    let rms: f64 = (samples
        .iter()
        .map(|&s| {
            let sample = s as f64;
            sample * sample
        })
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt();

    // Convert to decibels
    let db = 20.0 * rms.log10();

    // Map decibels to 0-100 range
    // Assuming -50 dB as the minimum audible level and 0 dB as the maximum
    let normalized_loudness = (db + 50.0) * (1.0 / 50.0);

    normalized_loudness.clamp(0.0, 1.0)
}
