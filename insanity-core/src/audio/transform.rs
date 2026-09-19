use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use crate::audio::chunk::AudioChunk;
use crate::audio::denoiser::{Denoiser, MultiChannelDenoiser};
use crate::audio::jitter::JitterBuffer;
use crate::audio::sample_ops::convert_to_mixer_channels;
use crate::loudness::calculate_loudness;
use crate::user_input_event::DenoiseSelection;

pub fn volume_multiplier(volume: usize) -> f32 {
    let vol = volume as f32;
    let a: f32 = 0.2;
    a * ((1.0 + 1.0 / a).powf(vol / 100.0) - 1.0)
}

pub trait ChunkTransform: Send {
    type OutputT;

    fn transform(&mut self, chunk: AudioChunk) -> Self::OutputT;

    fn chain<N: ChunkTransform>(self, next: N) -> Link<Self, N>
    where
        Self: Sized + ChunkTransform<OutputT = AudioChunk>,
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

impl<A: ChunkTransform<OutputT = AudioChunk>, B: ChunkTransform> ChunkTransform for Link<A, B> {
    type OutputT = B::OutputT;

    fn transform(&mut self, chunk: AudioChunk) -> Self::OutputT {
        let chunk = self.first.transform(chunk);
        self.second.transform(chunk)
    }
}

impl ChunkTransform for () {
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        chunk
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
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        let mut chunk = chunk;
        if self.control.is_muted() {
            chunk.audio_data.fill(0.0);
        }
        chunk
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
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        let volume = self.control.get();
        if volume == 100 {
            return chunk;
        }
        let multiplier = volume_multiplier(volume);
        let mut chunk = chunk;
        for sample in chunk.audio_data.iter_mut() {
            *sample *= multiplier;
        }
        chunk
    }
}

pub struct Clip {
    pub clip_hits: usize,
}

impl Clip {
    pub fn new() -> Self {
        Clip { clip_hits: 0 }
    }
}

impl Default for Clip {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkTransform for Clip {
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        let mut chunk = chunk;
        for sample in chunk.audio_data.iter_mut() {
            let clamped = (*sample).clamp(-1.0, 1.0);
            if clamped != *sample {
                self.clip_hits += 1;
            }
            *sample = clamped;
        }
        chunk
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
        *self
            .selection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = selection;
    }

    pub fn get(&self) -> DenoiseSelection {
        *self
            .selection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        match self.control.get() {
            DenoiseSelection::None => chunk,
            DenoiseSelection::Nnnoiseless => self.inner.denoise_chunk(chunk),
        }
    }
}

pub struct PassthroughOnQuiet<T> {
    inner: T,
    threshold: f32,
}

impl<T> PassthroughOnQuiet<T> {
    pub fn new(inner: T, threshold: f32) -> Self {
        PassthroughOnQuiet { inner, threshold }
    }
}

impl<T: ChunkTransform<OutputT = AudioChunk>> ChunkTransform for PassthroughOnQuiet<T> {
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        let quiet = chunk.audio_data.iter().all(|s| s.abs() < self.threshold);
        if quiet {
            chunk
        } else {
            self.inner.transform(chunk)
        }
    }
}

// Transform that applies an inner transform if present or recent chunk is loud.
// Similar to PassthroughOnQuiet but stateful.
pub struct SilenceGate<E, O> {
    inner: E,
    threshold: f32,
    hangover_chunks: usize,
    hangover_left: usize,
    marker: std::marker::PhantomData<O>,
}

impl<E, O> SilenceGate<E, O> {
    pub fn new(inner: E, threshold: f32, hangover_chunks: usize) -> Self {
        SilenceGate {
            inner,
            threshold,
            hangover_chunks,
            hangover_left: 0,
            marker: std::marker::PhantomData,
        }
    }

    fn should_send(&mut self, samples: &[f32]) -> bool {
        let quiet = samples.iter().all(|s| s.abs() < self.threshold);
        if !quiet {
            self.hangover_left = self.hangover_chunks;
            return true;
        }
        let send = self.hangover_left > 0;
        self.hangover_left = self.hangover_left.saturating_sub(1);
        send
    }
}

impl<E, O> ChunkTransform for SilenceGate<E, O>
where
    E: ChunkTransform<OutputT = Option<O>>,
    O: Send,
{
    type OutputT = Option<O>;

    // Returning None forces no further processing and no sending of this chunk
    fn transform(&mut self, chunk: AudioChunk) -> Option<O> {
        if self.should_send(&chunk.audio_data) {
            self.inner.transform(chunk)
        } else {
            None
        }
    }
}

pub struct MetricsState {
    loudness_bits: AtomicU64,
}

impl MetricsState {
    pub fn loudness(&self) -> f64 {
        f64::from_bits(self.loudness_bits.load(Ordering::Relaxed))
    }
}

impl Default for MetricsState {
    fn default() -> Self {
        MetricsState {
            loudness_bits: AtomicU64::new(0.0f64.to_bits()),
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
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        self.state.loudness_bits.store(
            calculate_loudness(&chunk.audio_data).to_bits(),
            Ordering::Relaxed,
        );
        chunk
    }
}

pub struct JitterStage {
    buffer: JitterBuffer<AudioChunk>,
    pub gap_detected: usize,
    pub late_dropped: usize,
    pub overflow_dropped: usize,
}

impl JitterStage {
    pub fn new(capacity_chunks: usize) -> Self {
        assert!(capacity_chunks > 0);
        JitterStage {
            buffer: JitterBuffer::new(capacity_chunks),
            gap_detected: 0,
            late_dropped: 0,
            overflow_dropped: 0,
        }
    }

    pub fn push(&mut self, chunk: AudioChunk) {
        let sequence = chunk.sequence_number;
        let (head, prev, empty) = (
            self.buffer.head(),
            self.buffer.prev(),
            self.buffer.is_empty(),
        );
        if sequence < head {
            self.late_dropped += 1;
        } else if empty {
            if sequence != head {
                self.gap_detected += 1;
            }
        } else if sequence > prev && sequence != prev + 1 {
            self.gap_detected += 1;
        }
        self.overflow_dropped += self.buffer.set(sequence, chunk);
    }

    pub fn reset(&mut self) {
        self.buffer.reset();
    }

    pub fn buffered_chunks(&self) -> usize {
        self.buffer.len()
    }

    pub fn pull(&mut self) -> Option<AudioChunk> {
        self.buffer.next_item()
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
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        let dst = if self.cap_channels {
            chunk.format.channel_count.min(self.dst_channels)
        } else {
            self.dst_channels
        };
        convert_to_mixer_channels(chunk, dst)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelMap, ChunkTransform, Clip, Denoise, DenoiseSelection, Gain, GainControl, Link,
        MetricsReader, Mute, PassthroughOnQuiet, SilenceGate, volume_multiplier,
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
    fn mute_passes_or_silences() {
        let (mut mute, control) = Mute::shared(false);
        let out = mute.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.sequence_number, 7);
        assert_eq!(out.audio_data, vec![0.5; 4]);
        control.set(true);
        assert!(control.is_muted());
        let out = mute.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.sequence_number, 7);
        assert_eq!(out.audio_data, vec![0.0; 4]);
    }

    #[test]
    fn gain_unity_passes_through_zero_silences() {
        let (mut gain, control) = Gain::shared(100, 500);
        let out = gain.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.5; 4]);
        control.set(0);
        assert_eq!(control.get(), 0);
        let out = gain.transform(chunk(vec![0.5; 4]));
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
    fn clip_passes_in_range_untouched() {
        let mut clip = Clip::new();
        let out = clip.transform(chunk(vec![-1.0, -0.5, 0.0, 0.5, 1.0]));
        assert_eq!(out.audio_data, vec![-1.0, -0.5, 0.0, 0.5, 1.0]);
        assert_eq!(clip.clip_hits, 0);
    }

    #[test]
    fn clip_clamps_and_counts_hits() {
        let mut clip = Clip::default();
        let out = clip.transform(chunk(vec![-2.0, 0.25, 1.5]));
        assert_eq!(out.audio_data, vec![-1.0, 0.25, 1.0]);
        assert_eq!(clip.clip_hits, 2);
    }

    #[test]
    fn denoise_none_passes_through() {
        let (mut denoise, control) = Denoise::<DoubleDenoiser>::shared(DenoiseSelection::None);
        let out = denoise.transform(chunk(vec![0.5; 8]));
        assert_eq!(out.audio_data, vec![0.5; 8]);
        control.set(DenoiseSelection::Nnnoiseless);
        let out = denoise.transform(chunk(vec![0.5; 8]));
        assert_eq!(out.audio_data, vec![1.0; 8]);
    }

    #[test]
    fn meter_records_and_passes_through() {
        let (mut meter, state) = MetricsReader::shared();
        let out = meter.transform(chunk(vec![0.5; 8]));
        assert_eq!(out.audio_data, vec![0.5; 8]);
        let loudness = state.loudness();
        assert!(loudness > 0.0 && loudness <= 1.0);
    }

    #[test]
    fn channel_map_matrix() {
        let mut map = ChannelMap::new(2);
        let out = map.transform(chunk_with(1, vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.5; 8]);
        assert_eq!(out.format.channel_count, 2);
        let mut map = ChannelMap::new(1);
        let out = map.transform(chunk(vec![0.25, 0.75]));
        assert_eq!(out.audio_data, vec![0.5]);
        assert_eq!(out.format.channel_count, 1);
        let mut map = ChannelMap::new(2);
        let out = map.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.5; 4]);
    }

    #[test]
    fn channel_map_capped_never_upmixes() {
        let mut map = ChannelMap::capped(2);
        let out = map.transform(chunk_with(1, vec![0.5; 4]));
        assert_eq!(out.format.channel_count, 1);
        assert_eq!(out.audio_data, vec![0.5; 4]);
        let out = map.transform(chunk(vec![0.25, 0.75]));
        assert_eq!(out.format.channel_count, 2);
        assert_eq!(out.audio_data, vec![0.25, 0.75]);
        let out = map.transform(chunk_with(3, vec![0.5; 6]));
        assert_eq!(out.format.channel_count, 2);
        assert_eq!(out.audio_data.len(), 4);
    }

    #[test]
    fn link_chains_in_order() {
        let (mute, mute_control) = Mute::shared(false);
        let (gain, _) = Gain::shared(100, 500);
        let mut chain: Link<Mute, Gain> = mute.chain(gain);
        let out = chain.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.5; 4]);
        mute_control.set(true);
        let out = chain.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.0; 4]);
    }

    #[test]
    fn unit_is_identity_and_nests() {
        let (gain, _) = Gain::shared(0, 500);
        let mut chain = ().chain(gain);
        let out = chain.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.0; 4]);
    }

    struct Counting {
        calls: usize,
    }

    impl ChunkTransform for Counting {
        type OutputT = AudioChunk;

        fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
            self.calls += 1;
            chunk
        }
    }

    #[test]
    fn quiet_passthrough_skips_inner() {
        let mut gate = PassthroughOnQuiet::new(Counting { calls: 0 }, 0.02);
        let out = gate.transform(chunk(vec![0.005; 8]));
        assert_eq!(out.sequence_number, 7);
        assert_eq!(out.audio_data, vec![0.005; 8]);
        assert_eq!(gate.inner.calls, 0);
    }

    #[test]
    fn loud_delegates_to_inner() {
        let mut gate = PassthroughOnQuiet::new(Counting { calls: 0 }, 0.02);
        let out = gate.transform(chunk(vec![0.4; 8]));
        assert_eq!(out.audio_data, vec![0.4; 8]);
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn threshold_boundary_runs_inner() {
        let mut gate = PassthroughOnQuiet::new(Counting { calls: 0 }, 0.02);
        let mut data = vec![0.019; 8];
        data[3] = 0.02;
        let out = gate.transform(chunk(data.clone()));
        assert_eq!(out.audio_data, data);
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn below_threshold_passes_through() {
        let mut gate = PassthroughOnQuiet::new(Counting { calls: 0 }, 0.02);
        let out = gate.transform(chunk(vec![0.019; 8]));
        assert_eq!(out.audio_data, vec![0.019; 8]);
        assert_eq!(gate.inner.calls, 0);
    }

    #[test]
    fn empty_chunk_passes_through() {
        let mut gate = PassthroughOnQuiet::new(Counting { calls: 0 }, 0.02);
        let out = gate.transform(chunk(Vec::new()));
        assert!(out.audio_data.is_empty());
        assert_eq!(gate.inner.calls, 0);
    }

    #[test]
    fn nan_runs_inner() {
        let mut gate = PassthroughOnQuiet::new(Counting { calls: 0 }, 0.02);
        let out = gate.transform(chunk(vec![0.0, f32::NAN, 0.0, 0.0]));
        assert_eq!(gate.inner.calls, 1);
        assert!(out.audio_data[1].is_nan());
    }

    #[test]
    fn gate_nests_in_chain() {
        let mut chain =
            PassthroughOnQuiet::new(Counting { calls: 0 }, 0.02).chain(Mute::shared(false).0);
        let out = chain.transform(chunk(vec![0.005; 4]));
        assert_eq!(out.audio_data, vec![0.005; 4]);
        let out = chain.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.5; 4]);
    }

    struct MaybeEncode {
        calls: usize,
        fail: bool,
    }

    impl ChunkTransform for MaybeEncode {
        type OutputT = Option<AudioChunk>;

        fn transform(&mut self, chunk: AudioChunk) -> Option<AudioChunk> {
            self.calls += 1;
            if self.fail { None } else { Some(chunk) }
        }
    }

    fn silence_gate() -> SilenceGate<MaybeEncode, AudioChunk> {
        SilenceGate::new(
            MaybeEncode {
                calls: 0,
                fail: false,
            },
            0.02,
            2,
        )
    }

    #[test]
    fn silence_gate_sends_loud_chunks() {
        let mut gate = silence_gate();
        let out = gate.transform(chunk(vec![0.4; 8]));
        assert_eq!(out.map(|c| c.sequence_number), Some(7));
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn silence_gate_skips_after_hangover() {
        let mut gate = silence_gate();
        gate.transform(chunk(vec![0.4; 8]));
        for _ in 0..2 {
            assert!(gate.transform(chunk(vec![0.0; 8])).is_some());
        }
        assert!(gate.transform(chunk(vec![0.0; 8])).is_none());
        assert!(gate.transform(chunk(vec![0.0; 8])).is_none());
        assert_eq!(gate.inner.calls, 3);
    }

    #[test]
    fn silence_gate_skips_leading_silence() {
        let mut gate = silence_gate();
        assert!(gate.transform(chunk(vec![0.0; 8])).is_none());
        assert_eq!(gate.inner.calls, 0);
    }

    #[test]
    fn silence_gate_speech_resets_hangover() {
        let mut gate = silence_gate();
        gate.transform(chunk(vec![0.4; 8]));
        gate.transform(chunk(vec![0.0; 8]));
        gate.transform(chunk(vec![0.4; 8]));
        for _ in 0..2 {
            assert!(gate.transform(chunk(vec![0.0; 8])).is_some());
        }
        assert!(gate.transform(chunk(vec![0.0; 8])).is_none());
    }

    #[test]
    fn silence_gate_nan_encodes() {
        let mut gate = silence_gate();
        let out = gate.transform(chunk(vec![0.0, f32::NAN, 0.0, 0.0]));
        assert!(out.is_some());
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn silence_gate_propagates_inner_failure() {
        let mut gate = SilenceGate::new(
            MaybeEncode {
                calls: 0,
                fail: true,
            },
            0.02,
            2,
        );
        assert!(gate.transform(chunk(vec![0.4; 8])).is_none());
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn silence_gate_reopens_after_close() {
        let mut gate = silence_gate();
        gate.transform(chunk(vec![0.4; 8]));
        for _ in 0..5 {
            gate.transform(chunk(vec![0.0; 8]));
        }
        assert!(gate.transform(chunk(vec![0.0; 8])).is_none());
        let out = gate.transform(chunk(vec![0.4; 8]));
        assert_eq!(out.map(|c| c.sequence_number), Some(7));
        for _ in 0..2 {
            assert!(gate.transform(chunk(vec![0.0; 8])).is_some());
        }
        assert!(gate.transform(chunk(vec![0.0; 8])).is_none());
    }
}
