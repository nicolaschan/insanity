use crate::audio::chunk::AudioChunk;

pub fn split_channels(samples: &[f32], channel_count: usize) -> Vec<Vec<f32>> {
    if channel_count == 0 {
        return Vec::new();
    }
    let frames = samples.len().div_ceil(channel_count);
    let mut channels: Vec<Vec<f32>> = (0..channel_count)
        .map(|_| Vec::with_capacity(frames))
        .collect();
    for (i, &sample) in samples.iter().enumerate() {
        channels[i % channel_count].push(sample);
    }
    channels
}

pub fn interleave_channels(channels: &[Vec<f32>]) -> Vec<f32> {
    let frame_size = channels.first().map(Vec::len).unwrap_or(0);
    let mut samples = Vec::with_capacity(frame_size * channels.len());
    for i in 0..frame_size {
        samples.extend(channels.iter().filter_map(|c| c.get(i).copied()));
    }
    samples
}

pub fn convert_to_mixer_channels(mut chunk: AudioChunk, mixer_channels: u16) -> AudioChunk {
    let src_channels = chunk.format.channel_count;
    if src_channels == mixer_channels || src_channels == 0 || mixer_channels == 0 {
        return chunk;
    }
    let frames = chunk.audio_data.len() / src_channels as usize;
    let dst = mixer_channels as usize;
    let mut out = Vec::with_capacity(frames * dst);
    let data: &[f32] = &chunk.audio_data;
    if src_channels == 1 && mixer_channels == 2 {
        out.extend(data.iter().flat_map(|&m| [m, m]));
    } else if src_channels == 2 && mixer_channels == 1 {
        let (pairs, _) = data.as_chunks::<2>();
        out.extend(pairs.iter().map(|pair| (pair[0] + pair[1]) * 0.5));
    } else {
        let src = src_channels as usize;
        out.extend((0..frames).flat_map(|f| (0..dst).map(move |t| data[f * src + t % src])));
    }
    chunk.audio_data = out;
    chunk.format.channel_count = mixer_channels;
    chunk
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
