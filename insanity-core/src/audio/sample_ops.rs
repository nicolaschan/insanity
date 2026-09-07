pub fn split_channels(samples: &[f32], channel_count: usize) -> Vec<Vec<f32>> {
    if channel_count == 0 {
        return Vec::new();
    }
    let mut channels: Vec<Vec<f32>> = vec![Vec::new(); channel_count];
    for (i, &sample) in samples.iter().enumerate() {
        let channel_index = i % channel_count;
        channels[channel_index].push(sample);
    }
    channels
}

pub fn interleave_channels(channels: &[Vec<f32>]) -> Vec<f32> {
    let mut samples = Vec::new();
    let frame_size = channels.first().map(Vec::len).unwrap_or(0);
    for i in 0..frame_size {
        for c in channels.iter() {
            samples.push(c[i]);
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::{interleave_channels, split_channels};

    #[test]
    fn separate_empty_count_returns_empty() {
        assert!(split_channels(&[0.1, 0.2], 0).is_empty());
        assert!(interleave_channels(&[]).is_empty());
    }

    #[test]
    fn separate_ragged_distributes_round_robin() {
        let out = split_channels(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], vec![0.0, 3.0, 6.0]);
        assert_eq!(out[1], vec![1.0, 4.0]);
        assert_eq!(out[2], vec![2.0, 5.0]);
    }

    #[test]
    fn separate_aligned_roundtrips_through_interleave() {
        let samples: Vec<f32> = (0..12).map(|v| v as f32).collect();
        let channels = split_channels(&samples, 3);
        assert_eq!(interleave_channels(&channels), samples);
    }
}
