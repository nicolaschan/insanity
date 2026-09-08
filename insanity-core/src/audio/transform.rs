use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use crate::audio::chunk::AudioChunk;
use crate::audio::denoiser::{Denoiser, MultiChannelDenoiser};
use crate::audio::sample_ops::convert_to_mixer_channels;
use crate::loudness::calculate_loudness;
use crate::user_input_event::DenoiseSelection;

pub fn volume_multiplier(volume: usize) -> f32 {
    let vol = volume as f32;
    let a: f32 = 0.2;
    a * ((1.0 + 1.0 / a).powf(vol / 100.0) - 1.0)
}

pub trait ChunkTransform: Send {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk>;

    fn chain<N: ChunkTransform>(self, next: N) -> Link<Self, N>
    where
        Self: Sized,
    {
        Link {
            first: self,
            second: next,
        }
    }
}

pub struct Link<A: ChunkTransform, B: ChunkTransform> {
    first: A,
    second: B,
}

impl<A: ChunkTransform, B: ChunkTransform> ChunkTransform for Link<A, B> {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
        self.first
            .transform(chunk)
            .and_then(|chunk| self.second.transform(chunk))
    }
}

impl ChunkTransform for () {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
        Some(chunk)
    }
}

pub struct MuteControl {
    muted: AtomicBool,
}

impl MuteControl {
    pub fn new(muted: bool) -> Self {
        MuteControl {
            muted: AtomicBool::new(muted),
        }
    }

    pub fn set(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

pub struct Mute {
    control: Arc<MuteControl>,
}

impl Mute {
    pub fn new(control: Arc<MuteControl>) -> Self {
        Mute { control }
    }

    pub fn shared(muted: bool) -> (Self, Arc<MuteControl>) {
        let control = Arc::new(MuteControl::new(muted));
        (Mute::new(control.clone()), control)
    }
}

impl ChunkTransform for Mute {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
        if self.control.is_muted() {
            None
        } else {
            Some(chunk)
        }
    }
}

pub struct GainControl {
    volume: AtomicUsize,
    max_volume: usize,
}

impl GainControl {
    pub fn new(volume: usize, max_volume: usize) -> Self {
        GainControl {
            volume: AtomicUsize::new(volume.min(max_volume)),
            max_volume,
        }
    }

    pub fn set(&self, volume: usize) {
        self.volume
            .store(volume.min(self.max_volume), Ordering::Relaxed);
    }

    pub fn get(&self) -> usize {
        self.volume.load(Ordering::Relaxed)
    }
}

pub struct Gain {
    control: Arc<GainControl>,
}

impl Gain {
    pub fn new(control: Arc<GainControl>) -> Self {
        Gain { control }
    }

    pub fn shared(volume: usize, max_volume: usize) -> (Self, Arc<GainControl>) {
        let control = Arc::new(GainControl::new(volume, max_volume));
        (Gain::new(control.clone()), control)
    }
}

impl ChunkTransform for Gain {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
        let volume = self.control.get();
        if volume == 100 {
            return Some(chunk);
        }
        let multiplier = volume_multiplier(volume);
        let mut chunk = chunk;
        for sample in chunk.audio_data.iter_mut() {
            *sample *= multiplier;
        }
        Some(chunk)
    }
}

pub struct DenoiseControl {
    selection: Mutex<DenoiseSelection>,
}

impl DenoiseControl {
    pub fn new(selection: DenoiseSelection) -> Self {
        DenoiseControl {
            selection: Mutex::new(selection),
        }
    }

    pub fn set(&self, selection: DenoiseSelection) {
        match self.selection.lock() {
            Ok(mut guard) => *guard = selection,
            Err(poisoned) => *poisoned.into_inner() = selection,
        }
    }

    pub fn get(&self) -> DenoiseSelection {
        match self.selection.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

pub struct Denoise<D: Denoiser> {
    control: Arc<DenoiseControl>,
    inner: MultiChannelDenoiser<D>,
}

impl<D: Denoiser> Denoise<D> {
    pub fn new(control: Arc<DenoiseControl>) -> Self {
        Denoise {
            control,
            inner: MultiChannelDenoiser::new(),
        }
    }

    pub fn shared(selection: DenoiseSelection) -> (Self, Arc<DenoiseControl>) {
        let control = Arc::new(DenoiseControl::new(selection));
        (Denoise::new(control.clone()), control)
    }
}

impl<D: Denoiser> ChunkTransform for Denoise<D> {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
        match self.control.get() {
            DenoiseSelection::None => Some(chunk),
            DenoiseSelection::Nnnoiseless => Some(self.inner.denoise_chunk(&chunk)),
        }
    }
}

pub struct MetricsState {
    loudness_bits: AtomicU64,
    frames: AtomicUsize,
}

impl MetricsState {
    pub fn loudness(&self) -> f64 {
        f64::from_bits(self.loudness_bits.load(Ordering::Relaxed))
    }

    pub fn frames(&self) -> usize {
        self.frames.load(Ordering::Relaxed)
    }
}

impl Default for MetricsState {
    fn default() -> Self {
        MetricsState {
            loudness_bits: AtomicU64::new(0.0f64.to_bits()),
            frames: AtomicUsize::new(0),
        }
    }
}

pub struct MetricsReader {
    state: Arc<MetricsState>,
}

impl MetricsReader {
    pub fn new(state: Arc<MetricsState>) -> Self {
        MetricsReader { state }
    }

    pub fn shared() -> (Self, Arc<MetricsState>) {
        let state = Arc::new(MetricsState::default());
        (MetricsReader::new(state.clone()), state)
    }
}

impl ChunkTransform for MetricsReader {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
        self.state.loudness_bits.store(
            calculate_loudness(&chunk.audio_data).to_bits(),
            Ordering::Relaxed,
        );
        self.state.frames.fetch_add(1, Ordering::Relaxed);
        Some(chunk)
    }
}

pub struct ChannelMap {
    dst_channels: u16,
    cap_channels: bool,
}

impl ChannelMap {
    pub fn new(dst_channels: u16) -> Self {
        ChannelMap {
            dst_channels,
            cap_channels: false,
        }
    }

    pub fn capped(max_channels: u16) -> Self {
        ChannelMap {
            dst_channels: max_channels,
            cap_channels: true,
        }
    }
}

impl ChunkTransform for ChannelMap {
    fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
        let dst = if self.cap_channels {
            chunk.format.channel_count.min(self.dst_channels)
        } else {
            self.dst_channels
        };
        Some(convert_to_mixer_channels(chunk, dst))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelMap, ChunkTransform, Denoise, DenoiseSelection, Gain, GainControl, Link,
        MetricsReader, Mute, volume_multiplier,
    };
    use crate::audio::AudioFormat;
    use crate::audio::chunk::AudioChunk;
    use crate::audio::denoiser::Denoiser;

    struct DoubleDenoiser;

    impl Denoiser for DoubleDenoiser {
        const FRAME_SIZE: usize = 4;

        fn init() -> Self {
            DoubleDenoiser
        }

        fn process_frame(&mut self, output: &mut [f32], input: &[f32]) {
            for (o, i) in output.iter_mut().zip(input.iter()) {
                *o = *i * 2.0;
            }
        }
    }

    fn chunk(data: Vec<f32>) -> AudioChunk {
        chunk_with(2, data)
    }

    fn chunk_with(channels: u16, data: Vec<f32>) -> AudioChunk {
        AudioChunk::new(7, AudioFormat::new(channels, 48000), data)
    }

    #[test]
    fn mute_passes_or_drops() {
        let (mut mute, control) = Mute::shared(false);
        let out = mute.transform(chunk(vec![0.5; 4])).expect("live");
        assert_eq!(out.sequence_number, 7);
        assert_eq!(out.audio_data, vec![0.5; 4]);
        control.set(true);
        assert!(control.is_muted());
        assert!(mute.transform(chunk(vec![0.5; 4])).is_none());
    }

    #[test]
    fn gain_unity_passes_through_zero_silences() {
        let (mut gain, control) = Gain::shared(100, 500);
        let out = gain.transform(chunk(vec![0.5; 4])).expect("live");
        assert_eq!(out.audio_data, vec![0.5; 4]);
        control.set(0);
        assert_eq!(control.get(), 0);
        let out = gain.transform(chunk(vec![0.5; 4])).expect("live");
        assert_eq!(out.audio_data, vec![0.0; 4]);
    }

    #[test]
    fn gain_clamps_at_max() {
        let control = GainControl::new(999, 500);
        assert_eq!(control.get(), 500);
    }

    #[test]
    fn volume_curve_contract() {
        assert_eq!(volume_multiplier(0), 0.0);
        assert!((volume_multiplier(100) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn denoise_none_passes_through() {
        let (mut denoise, control) = Denoise::<DoubleDenoiser>::shared(DenoiseSelection::None);
        let out = denoise.transform(chunk(vec![0.5; 8])).expect("live");
        assert_eq!(out.audio_data, vec![0.5; 8]);
        control.set(DenoiseSelection::Nnnoiseless);
        let out = denoise.transform(chunk(vec![0.5; 8])).expect("live");
        assert_eq!(out.audio_data, vec![1.0; 8]);
    }

    #[test]
    fn meter_records_and_passes_through() {
        let (mut meter, state) = MetricsReader::shared();
        let out = meter.transform(chunk(vec![0.5; 8])).expect("live");
        assert_eq!(out.audio_data, vec![0.5; 8]);
        assert_eq!(state.frames(), 1);
        let loudness = state.loudness();
        assert!(loudness > 0.0 && loudness <= 1.0);
    }

    #[test]
    fn channel_map_matrix() {
        let mut map = ChannelMap::new(2);
        let out = map.transform(chunk_with(1, vec![0.5; 4])).expect("live");
        assert_eq!(out.audio_data, vec![0.5; 8]);
        assert_eq!(out.format.channel_count, 2);
        let mut map = ChannelMap::new(1);
        let out = map.transform(chunk(vec![0.25, 0.75])).expect("live");
        assert_eq!(out.audio_data, vec![0.5]);
        assert_eq!(out.format.channel_count, 1);
        let mut map = ChannelMap::new(2);
        let out = map.transform(chunk(vec![0.5; 4])).expect("live");
        assert_eq!(out.audio_data, vec![0.5; 4]);
    }

    #[test]
    fn channel_map_capped_never_upmixes() {
        let mut map = ChannelMap::capped(2);
        let out = map.transform(chunk_with(1, vec![0.5; 4])).expect("live");
        assert_eq!(out.format.channel_count, 1);
        assert_eq!(out.audio_data, vec![0.5; 4]);
        let out = map.transform(chunk(vec![0.25, 0.75])).expect("live");
        assert_eq!(out.format.channel_count, 2);
        assert_eq!(out.audio_data, vec![0.25, 0.75]);
        let out = map.transform(chunk_with(3, vec![0.5; 6])).expect("live");
        assert_eq!(out.format.channel_count, 2);
        assert_eq!(out.audio_data.len(), 4);
    }

    #[test]
    fn link_chains_drop_short_circuits() {
        let (mute, mute_control) = Mute::shared(false);
        let (gain, _) = Gain::shared(100, 500);
        let mut chain: Link<Mute, Gain> = mute.chain(gain);
        assert!(chain.transform(chunk(vec![0.5; 4])).is_some());
        mute_control.set(true);
        assert!(chain.transform(chunk(vec![0.5; 4])).is_none());
    }

    #[test]
    fn unit_is_identity_and_nests() {
        let (gain, _) = Gain::shared(0, 500);
        let mut chain = ().chain(gain);
        let out = chain.transform(chunk(vec![0.5; 4])).expect("live");
        assert_eq!(out.audio_data, vec![0.0; 4]);
    }
}
