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

pub const POP_THRESHOLD: f32 = 0.25;
const POP_SLOPE_RATIO: f32 = 6.0;
const SLOPE_ALPHA: f32 = 1.0 / 64.0;
const RAMP_MS: usize = 5;

#[derive(Clone, Copy, Default)]
struct DepopChannel {
    last: f32,
    offset_start: f32,
    remaining: usize,
    slope: f32,
}

impl DepopChannel {
    fn offset(&self, ramp: usize) -> f32 {
        if self.remaining == 0 {
            return 0.0;
        }
        self.offset_start * self.remaining as f32 / ramp as f32
    }

    fn next(&mut self, sample: f32, ramp: usize, pops: &mut usize) -> f32 {
        if !sample.is_finite() {
            return sample;
        }
        let delta = sample - self.last;
        self.last = sample;
        let magnitude = delta.abs();
        if magnitude > POP_THRESHOLD && magnitude > POP_SLOPE_RATIO * self.slope {
            self.offset_start = self.offset(ramp) - delta;
            self.remaining = ramp;
            *pops += 1;
        }
        self.slope += (magnitude - self.slope) * SLOPE_ALPHA;
        let out = sample + self.offset(ramp);
        self.remaining = self.remaining.saturating_sub(1);
        out
    }
}

pub struct Depop {
    ramp: usize,
    channels: Vec<DepopChannel>,
    pub pops: usize,
}

impl Depop {
    pub fn new(ramp_frames: usize) -> Self {
        assert!(ramp_frames > 0, "ramp_frames must be > 0");
        Depop {
            ramp: ramp_frames,
            channels: Vec::new(),
            pops: 0,
        }
    }

    pub fn ramp_frames(sample_rate: u32) -> usize {
        (sample_rate as usize * RAMP_MS / 1000).max(1)
    }
}

impl ChunkTransform for Depop {
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        let mut chunk = chunk;
        let channels = chunk.format.channel_count as usize;
        if channels == 0 {
            return chunk;
        }
        if self.channels.len() != channels {
            self.channels = vec![DepopChannel::default(); channels];
        }
        let Depop {
            ramp,
            channels: states,
            pops,
        } = self;
        for frame in chunk.audio_data.chunks_mut(channels) {
            for (sample, state) in frame.iter_mut().zip(states.iter_mut()) {
                *sample = state.next(*sample, *ramp, pops);
            }
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

pub trait Detector: Send {
    fn detect(&mut self, samples: &[f32]) -> bool;
}

pub struct PeakDetector {
    threshold: f32,
}

impl PeakDetector {
    pub fn new(threshold: f32) -> Self {
        PeakDetector { threshold }
    }
}

impl Detector for PeakDetector {
    fn detect(&mut self, samples: &[f32]) -> bool {
        !samples.iter().all(|s| s.abs() < self.threshold)
    }
}

pub struct RmsDetector {
    threshold: f32,
}

impl RmsDetector {
    pub fn new(threshold: f32) -> Self {
        RmsDetector { threshold }
    }
}

impl Detector for RmsDetector {
    fn detect(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return false;
        }
        let mean_square = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
        let rms = mean_square.sqrt();
        rms.is_nan() || rms >= self.threshold
    }
}

pub struct Hysteresis<D> {
    inner: D,
    chunks: usize,
    left: usize,
}

impl<D> Hysteresis<D> {
    pub fn new(inner: D, chunks: usize) -> Self {
        Hysteresis {
            inner,
            chunks,
            left: 0,
        }
    }
}

impl<D: Detector> Detector for Hysteresis<D> {
    fn detect(&mut self, samples: &[f32]) -> bool {
        if self.inner.detect(samples) {
            self.left = self.chunks;
            return true;
        }
        let active = self.left > 0;
        self.left = self.left.saturating_sub(1);
        active
    }
}

pub struct PassthroughGate<T, D> {
    inner: T,
    detector: D,
}

impl<T, D> PassthroughGate<T, D> {
    pub fn new(inner: T, detector: D) -> Self {
        PassthroughGate { inner, detector }
    }
}

impl<T, D> ChunkTransform for PassthroughGate<T, D>
where
    T: ChunkTransform<OutputT = AudioChunk>,
    D: Detector,
{
    type OutputT = AudioChunk;

    fn transform(&mut self, chunk: AudioChunk) -> AudioChunk {
        if self.detector.detect(&chunk.audio_data) {
            self.inner.transform(chunk)
        } else {
            chunk
        }
    }
}

pub struct SilenceGate<E, O, D> {
    inner: E,
    detector: D,
    marker: std::marker::PhantomData<O>,
}

impl<E, O, D> SilenceGate<E, O, D> {
    pub fn new(inner: E, detector: D) -> Self {
        SilenceGate {
            inner,
            detector,
            marker: std::marker::PhantomData,
        }
    }
}

impl<E, O, D> ChunkTransform for SilenceGate<E, O, D>
where
    E: ChunkTransform<OutputT = Option<O>>,
    O: Send,
    D: Detector,
{
    type OutputT = Option<O>;

    fn transform(&mut self, chunk: AudioChunk) -> Option<O> {
        if self.detector.detect(&chunk.audio_data) {
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
        ChannelMap, ChunkTransform, Clip, Denoise, DenoiseSelection, Depop, Detector, Gain,
        GainControl, Hysteresis, Link, MetricsReader, Mute, POP_THRESHOLD, PassthroughGate,
        PeakDetector, RmsDetector, SilenceGate, volume_multiplier,
    };
    use crate::audio::AudioFormat;
    use crate::audio::chunk::AudioChunk;
    use crate::audio::denoiser::Denoiser;
    use std::collections::VecDeque;

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
        let mut gate = PassthroughGate::new(Counting { calls: 0 }, PeakDetector::new(0.02));
        let out = gate.transform(chunk(vec![0.005; 8]));
        assert_eq!(out.sequence_number, 7);
        assert_eq!(out.audio_data, vec![0.005; 8]);
        assert_eq!(gate.inner.calls, 0);
    }

    #[test]
    fn loud_delegates_to_inner() {
        let mut gate = PassthroughGate::new(Counting { calls: 0 }, PeakDetector::new(0.02));
        let out = gate.transform(chunk(vec![0.4; 8]));
        assert_eq!(out.audio_data, vec![0.4; 8]);
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn threshold_boundary_runs_inner() {
        let mut gate = PassthroughGate::new(Counting { calls: 0 }, PeakDetector::new(0.02));
        let mut data = vec![0.019; 8];
        data[3] = 0.02;
        let out = gate.transform(chunk(data.clone()));
        assert_eq!(out.audio_data, data);
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn below_threshold_passes_through() {
        let mut gate = PassthroughGate::new(Counting { calls: 0 }, PeakDetector::new(0.02));
        let out = gate.transform(chunk(vec![0.019; 8]));
        assert_eq!(out.audio_data, vec![0.019; 8]);
        assert_eq!(gate.inner.calls, 0);
    }

    #[test]
    fn empty_chunk_passes_through() {
        let mut gate = PassthroughGate::new(Counting { calls: 0 }, PeakDetector::new(0.02));
        let out = gate.transform(chunk(Vec::new()));
        assert!(out.audio_data.is_empty());
        assert_eq!(gate.inner.calls, 0);
    }

    #[test]
    fn nan_runs_inner() {
        let mut gate = PassthroughGate::new(Counting { calls: 0 }, PeakDetector::new(0.02));
        let out = gate.transform(chunk(vec![0.0, f32::NAN, 0.0, 0.0]));
        assert_eq!(gate.inner.calls, 1);
        assert!(out.audio_data[1].is_nan());
    }

    #[test]
    fn gate_nests_in_chain() {
        let mut chain = PassthroughGate::new(Counting { calls: 0 }, PeakDetector::new(0.02))
            .chain(Mute::shared(false).0);
        let out = chain.transform(chunk(vec![0.005; 4]));
        assert_eq!(out.audio_data, vec![0.005; 4]);
        let out = chain.transform(chunk(vec![0.5; 4]));
        assert_eq!(out.audio_data, vec![0.5; 4]);
    }

    #[test]
    fn passthrough_hangover_runs_inner_after_speech() {
        let mut gate = PassthroughGate::new(
            Counting { calls: 0 },
            Hysteresis::new(PeakDetector::new(0.02), 2),
        );
        gate.transform(chunk(vec![0.4; 8]));
        for _ in 0..2 {
            gate.transform(chunk(vec![0.0; 8]));
        }
        assert_eq!(gate.inner.calls, 3);
        gate.transform(chunk(vec![0.0; 8]));
        assert_eq!(gate.inner.calls, 3);
    }

    struct ScriptedDetector {
        script: VecDeque<bool>,
    }

    impl Detector for ScriptedDetector {
        fn detect(&mut self, _samples: &[f32]) -> bool {
            self.script.pop_front().unwrap_or(false)
        }
    }

    #[test]
    fn hangover_counts_down_silence() {
        let mut hangover = Hysteresis::new(PeakDetector::new(0.02), 2);
        assert!(hangover.detect(&[0.4; 8]));
        assert!(hangover.detect(&[0.0; 8]));
        assert!(hangover.detect(&[0.0; 8]));
        assert!(!hangover.detect(&[0.0; 8]));
    }

    #[test]
    fn hangover_loud_resets_countdown() {
        let mut hangover = Hysteresis::new(PeakDetector::new(0.02), 2);
        assert!(hangover.detect(&[0.4; 8]));
        assert!(hangover.detect(&[0.0; 8]));
        assert!(hangover.detect(&[0.4; 8]));
        assert!(hangover.detect(&[0.0; 8]));
        assert!(hangover.detect(&[0.0; 8]));
        assert!(!hangover.detect(&[0.0; 8]));
    }

    #[test]
    fn hangover_composes_over_stub_detector() {
        let stub = ScriptedDetector {
            script: VecDeque::from([true, false, false, false]),
        };
        let mut hangover = Hysteresis::new(stub, 2);
        assert!(hangover.detect(&[0.0; 4]));
        assert!(hangover.detect(&[0.0; 4]));
        assert!(hangover.detect(&[0.0; 4]));
        assert!(!hangover.detect(&[0.0; 4]));
    }

    #[test]
    fn rms_gate_detects_energy_not_peaks() {
        let mut gate = RmsDetector::new(0.01);
        assert!(!gate.detect(&[0.0; 960]));
        assert!(gate.detect(&[0.4; 960]));
        assert!(!gate.detect(&[]));
    }

    #[test]
    fn rms_gate_sine_boundary() {
        let sine: Vec<f32> = (0..960)
            .map(|i| (440.0 * i as f32 / 48000.0 * std::f32::consts::TAU).sin() * 0.02)
            .collect();
        let mut gate = RmsDetector::new(0.01);
        assert!(gate.detect(&sine));
        let mut strict = RmsDetector::new(0.02);
        assert!(!strict.detect(&sine));
    }

    #[test]
    fn rms_gate_nan_detects() {
        let mut gate = RmsDetector::new(0.01);
        assert!(gate.detect(&[0.0, f32::NAN, 0.0, 0.0]));
    }

    #[test]
    fn rms_gate_ignores_single_spike() {
        let mut data = vec![0.0; 960];
        data[100] = 0.5;
        let mut rms = RmsDetector::new(0.05);
        assert!(!rms.detect(&data));
        let mut peak = PeakDetector::new(0.02);
        assert!(peak.detect(&data));
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

    fn silence_gate() -> SilenceGate<MaybeEncode, AudioChunk, Hysteresis<PeakDetector>> {
        SilenceGate::new(
            MaybeEncode {
                calls: 0,
                fail: false,
            },
            Hysteresis::new(PeakDetector::new(0.02), 2),
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
            Hysteresis::new(PeakDetector::new(0.02), 2),
        );
        assert!(gate.transform(chunk(vec![0.4; 8])).is_none());
        assert_eq!(gate.inner.calls, 1);
    }

    #[test]
    fn silence_gate_follows_stub_detector() {
        let mut gate = SilenceGate::new(
            MaybeEncode {
                calls: 0,
                fail: false,
            },
            ScriptedDetector {
                script: VecDeque::from([true, false]),
            },
        );
        assert!(gate.transform(chunk(vec![0.0; 8])).is_some());
        assert!(gate.transform(chunk(vec![0.0; 8])).is_none());
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

    fn assert_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
            assert!((a - e).abs() < 1e-6, "sample {i}: {a} vs {e}");
        }
    }

    fn max_step(samples: &[f32], channels: usize) -> f32 {
        (0..channels)
            .map(|c| {
                samples
                    .iter()
                    .skip(c)
                    .step_by(channels)
                    .fold((0.0f32, 0.0f32), |(last, max), s| {
                        (*s, max.max((s - last).abs()))
                    })
                    .1
            })
            .fold(0.0, f32::max)
    }

    #[test]
    fn depop_ramp_frames_from_rate() {
        assert_eq!(Depop::ramp_frames(48000), 240);
        assert_eq!(Depop::ramp_frames(8000), 40);
        assert_eq!(Depop::ramp_frames(1), 1);
    }

    #[test]
    fn depop_passes_continuous_signal_untouched() {
        let sine: Vec<f32> = (0..960)
            .map(|i| (440.0 * (i / 2) as f32 / 48000.0 * std::f32::consts::TAU).sin() * 0.9)
            .collect();
        let mut depop = Depop::new(240);
        let out = depop.transform(chunk(sine.clone()));
        assert_eq!(out.audio_data, sine);
        assert_eq!(out.sequence_number, 7);
        assert_eq!(depop.pops, 0);
    }

    #[test]
    fn depop_bridges_step_up_from_silence() {
        let mut depop = Depop::new(4);
        let out = depop.transform(chunk_with(1, vec![0.0, 0.0, 0.8, 0.8, 0.8, 0.8, 0.8, 0.8]));
        assert_close(&out.audio_data, &[0.0, 0.0, 0.0, 0.2, 0.4, 0.6, 0.8, 0.8]);
        assert_eq!(depop.pops, 1);
    }

    #[test]
    fn depop_bridges_step_down_across_chunks() {
        let mut depop = Depop::new(4);
        depop.transform(chunk_with(1, vec![0.8; 8]));
        let out = depop.transform(chunk_with(1, vec![0.0; 6]));
        assert_close(&out.audio_data, &[0.8, 0.6, 0.4, 0.2, 0.0, 0.0]);
        assert_eq!(depop.pops, 2);
    }

    #[test]
    fn depop_step_during_ramp_accumulates() {
        let mut depop = Depop::new(4);
        depop.transform(chunk_with(1, vec![0.8; 8]));
        let out = depop.transform(chunk_with(1, vec![0.0, 0.0, 0.8, 0.8, 0.8, 0.8, 0.8]));
        assert_close(&out.audio_data, &[0.8, 0.6, 0.4, 0.5, 0.6, 0.7, 0.8]);
        assert_eq!(depop.pops, 3);
    }

    #[test]
    fn depop_channels_are_independent() {
        let mut depop = Depop::new(2);
        let out = depop.transform(chunk(vec![0.0, 0.0, 0.8, 0.0, 0.8, 0.0, 0.8, 0.0]));
        assert_close(&out.audio_data, &[0.0, 0.0, 0.0, 0.0, 0.4, 0.0, 0.8, 0.0]);
        assert_eq!(depop.pops, 1);
    }

    #[test]
    fn depop_small_steps_pass_untouched() {
        let mut depop = Depop::new(4);
        let data = vec![0.0, 0.2, 0.2, 0.0, 0.0, -0.2, 0.0];
        let out = depop.transform(chunk_with(1, data.clone()));
        assert_eq!(out.audio_data, data);
        assert_eq!(depop.pops, 0);
    }

    #[test]
    fn depop_ignores_loud_high_frequency_content() {
        let data: Vec<f32> = (0..960)
            .map(|i| {
                let envelope = (i as f32 / 240.0).min(1.0);
                (6000.0 * i as f32 / 48000.0 * std::f32::consts::TAU).sin() * 0.5 * envelope
            })
            .collect();
        assert!(max_step(&data, 1) > POP_THRESHOLD);
        let mut depop = Depop::new(240);
        let out = depop.transform(chunk_with(1, data.clone()));
        assert_eq!(out.audio_data, data);
        assert_eq!(depop.pops, 0);
    }

    #[test]
    fn depop_output_never_jumps_and_converges() {
        let mut data = Vec::new();
        for level in [0.0, 0.9, -0.7, 0.0, 0.5] {
            data.extend(std::iter::repeat_n(level, 480));
        }
        let mut depop = Depop::new(240);
        let out = depop.transform(chunk_with(1, data.clone()));
        assert!(max_step(&out.audio_data, 1) < 0.01);
        assert_close(&out.audio_data[2160..], &data[2160..]);
        assert_eq!(depop.pops, 4);
    }

    #[test]
    fn depop_non_finite_passes_through_and_recovers() {
        let mut depop = Depop::new(4);
        let out = depop.transform(chunk_with(
            1,
            vec![0.0, f32::NAN, 0.0, 0.8, 0.8, 0.8, 0.8, 0.8],
        ));
        assert!(out.audio_data[1].is_nan());
        assert_close(&out.audio_data[2..], &[0.0, 0.0, 0.2, 0.4, 0.6, 0.8]);
        assert_eq!(depop.pops, 1);
    }
}
