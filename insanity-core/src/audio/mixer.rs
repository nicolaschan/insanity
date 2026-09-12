use std::collections::{HashMap, VecDeque};

use crate::audio::AudioFormat;
use crate::audio::chunk::AudioChunk;
use crate::audio::codec::{AudioDecoder, EncodedChunk};
use crate::audio::jitter::JitterBuffer;
use crate::audio::sample::{Resampler, SampleSource, SyncSampleSource};
use crate::audio::transform::{ChannelMap, ChunkTransform, Clip};

pub const DEFAULT_JITTER_CHUNKS: usize = 10;
pub const PLC_FADE_SAMPLES: usize = 960;
pub const DEFAULT_OUT_FRAMES: usize = 480;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MixerMetrics {
    pub gap_detected: usize,
    pub late_dropped: usize,
    pub underrun: usize,
    pub plc_hold: usize,
    pub clip_hits: usize,
    pub fills: usize,
}

pub trait MixerInput {
    fn push_frame(&mut self, frame: EncodedChunk);
}

pub(crate) struct ChunkDecoder<D: AudioDecoder, F: FnMut(&AudioFormat) -> Option<D>> {
    decoder: Option<D>,
    decoder_format: Option<AudioFormat>,
    rebuild: F,
}

impl<D: AudioDecoder, F: FnMut(&AudioFormat) -> Option<D>> ChunkDecoder<D, F> {
    pub fn new(rebuild: F) -> Self {
        ChunkDecoder {
            decoder: None,
            decoder_format: None,
            rebuild,
        }
    }

    pub fn decode_frame(&mut self, frame: &EncodedChunk) -> Option<AudioChunk> {
        if self.decoder_format.as_ref() != Some(&frame.format) {
            let decoder = (self.rebuild)(&frame.format)?;
            self.decoder = Some(decoder);
            self.decoder_format = Some(frame.format.clone());
        }
        self.decoder
            .as_mut()
            .and_then(|decoder| decoder.decode(frame))
    }
}

pub(crate) struct JitterStage {
    buffer: JitterBuffer<AudioChunk>,
    pub gap_detected: usize,
    pub late_dropped: usize,
}

impl JitterStage {
    pub fn new(capacity_chunks: usize) -> Self {
        assert!(capacity_chunks > 0);
        JitterStage {
            buffer: JitterBuffer::new(capacity_chunks),
            gap_detected: 0,
            late_dropped: 0,
        }
    }

    pub fn push(&mut self, chunk: AudioChunk) {
        let sequence = chunk.sequence_number;
        let virgin = self.buffer.is_empty() && self.buffer.head() == 0 && self.buffer.prev() == 0;
        if sequence < self.buffer.head() {
            self.late_dropped += 1;
        } else if virgin {
            if sequence != 0 {
                self.gap_detected += 1;
            }
        } else if sequence > self.buffer.prev() && sequence != self.buffer.prev() + 1 {
            self.gap_detected += 1;
        }
        self.buffer.set(sequence, chunk);
    }

    pub fn pull(&mut self) -> Option<AudioChunk> {
        self.buffer.next_item()
    }
}

pub(crate) struct Conceal {
    last_sample: f32,
    fade_start: f32,
    fade_pos: usize,
    pub underrun: usize,
    pub plc_hold: usize,
}

impl Conceal {
    pub fn new() -> Self {
        Conceal {
            last_sample: 0.0,
            fade_start: 0.0,
            fade_pos: 0,
            underrun: 0,
            plc_hold: 0,
        }
    }

    pub fn next(&mut self, sample: Option<f32>) -> f32 {
        if self.fade_pos != 0 {
            self.plc_hold += 1;
            let position = self.fade_pos as f32 / PLC_FADE_SAMPLES as f32;
            let output = if self.fade_pos < PLC_FADE_SAMPLES {
                self.fade_start * (1.0 - position)
            } else {
                0.0
            };
            self.fade_pos += 1;
            if self.fade_pos >= PLC_FADE_SAMPLES {
                self.fade_pos = 0;
                self.last_sample = 0.0;
            }
            return output;
        }
        match sample {
            Some(value) => {
                self.last_sample = value;
                self.fade_start = value;
                value
            }
            None => {
                self.underrun += 1;
                self.plc_hold += 1;
                self.fade_start = self.last_sample;
                self.fade_pos = 1;
                self.fade_start
            }
        }
    }
}

impl Default for Conceal {
    fn default() -> Self {
        Self::new()
    }
}

pub struct InputSlot<D, T, R, FD>
where
    D: AudioDecoder,
    T: ChunkTransform,
    R: Resampler,
    FD: FnMut(&AudioFormat) -> Option<D>,
{
    decoder: ChunkDecoder<D, FD>,
    channel_map: ChannelMap,
    transform: T,
    jitter: JitterStage,
    resampler: R,
    conceal: Conceal,
}

impl<D, T, R, FD> MixerInput for InputSlot<D, T, R, FD>
where
    D: AudioDecoder,
    T: ChunkTransform,
    R: Resampler,
    FD: FnMut(&AudioFormat) -> Option<D>,
{
    fn push_frame(&mut self, frame: EncodedChunk) {
        let Some(decoded) = self.decoder.decode_frame(&frame) else {
            return;
        };
        let Some(converted) = self.channel_map.transform(decoded) else {
            return;
        };
        let Some(processed) = self.transform.transform(converted) else {
            return;
        };
        self.jitter.push(processed);
    }
}

pub struct Mixer<D, T, R, M, FD>
where
    D: AudioDecoder,
    T: ChunkTransform,
    R: Resampler,
    M: ChunkTransform,
    FD: FnMut(&AudioFormat) -> Option<D> + Send,
{
    slots: HashMap<u32, InputSlot<D, T, R, FD>>,
    bus: M,
    clip: Clip,
    pending: VecDeque<f32>,
    out_format: AudioFormat,
    out_frames: usize,
    out_sequence: u128,
    jitter_chunks: usize,
    fills: usize,
    next_id: u32,
}

impl<D, T, R, M, FD> Mixer<D, T, R, M, FD>
where
    D: AudioDecoder,
    T: ChunkTransform,
    R: Resampler,
    M: ChunkTransform,
    FD: FnMut(&AudioFormat) -> Option<D> + Send,
{
    pub fn new(out_format: AudioFormat, jitter_chunks: usize, out_frames: usize, bus: M) -> Self {
        Mixer {
            slots: HashMap::new(),
            bus,
            clip: Clip::new(),
            pending: VecDeque::new(),
            out_format,
            out_frames: out_frames.max(1),
            out_sequence: 0,
            jitter_chunks: jitter_chunks.max(1),
            fills: 0,
            next_id: 0,
        }
    }

    pub fn subscribe(&mut self, transform: T, rebuild: FD, resampler: R) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let slot = InputSlot {
            decoder: ChunkDecoder::new(rebuild),
            channel_map: ChannelMap::new(self.out_format.channel_count),
            transform,
            jitter: JitterStage::new(self.jitter_chunks),
            resampler,
            conceal: Conceal::new(),
        };
        self.slots.insert(id, slot);
        id
    }

    pub fn unsubscribe(&mut self, id: u32) {
        self.slots.remove(&id);
    }

    pub fn input_mut(&mut self, id: u32) -> Option<&mut InputSlot<D, T, R, FD>> {
        self.slots.get_mut(&id)
    }

    pub fn peer_count(&self) -> usize {
        self.slots.len()
    }

    pub fn metrics_snapshot(&self) -> MixerMetrics {
        let mut metrics = MixerMetrics {
            clip_hits: self.clip.clip_hits,
            fills: self.fills,
            ..MixerMetrics::default()
        };
        for slot in self.slots.values() {
            metrics.gap_detected += slot.jitter.gap_detected;
            metrics.late_dropped += slot.jitter.late_dropped;
            metrics.underrun += slot.conceal.underrun;
            metrics.plc_hold += slot.conceal.plc_hold;
        }
        metrics
    }

    fn refill(&mut self) {
        self.fills += 1;
        let channels = self.out_format.channel_count as usize;
        let needed = self.out_frames * channels;
        let mut mixed = vec![0.0; needed];
        for slot in self.slots.values_mut() {
            while slot.resampler.buffered() < needed {
                let Some(ordered) = slot.jitter.pull() else {
                    break;
                };
                for sample in ordered.audio_data {
                    slot.resampler.push_sample(sample);
                }
            }
            for sample in mixed.iter_mut() {
                *sample += slot.conceal.next(slot.resampler.pop_sample());
            }
        }
        let sequence = self.out_sequence;
        self.out_sequence += 1;
        let chunk = AudioChunk::new(sequence, self.out_format.clone(), mixed);
        let Some(converted) = self.bus.transform(chunk) else {
            return;
        };
        if let Some(output) = self.clip.transform(converted) {
            self.pending.extend(output.audio_data);
        }
    }
}

impl<D, T, R, M, FD> SampleSource for Mixer<D, T, R, M, FD>
where
    D: AudioDecoder,
    T: ChunkTransform,
    R: Resampler,
    M: ChunkTransform,
    FD: FnMut(&AudioFormat) -> Option<D> + Send,
{
    async fn next(&mut self) -> Option<f32> {
        self.next_sync()
    }
}

impl<D, T, R, M, FD> SyncSampleSource for Mixer<D, T, R, M, FD>
where
    D: AudioDecoder,
    T: ChunkTransform,
    R: Resampler,
    M: ChunkTransform,
    FD: FnMut(&AudioFormat) -> Option<D> + Send,
{
    fn next_sync(&mut self) -> Option<f32> {
        if self.slots.is_empty() {
            return Some(0.0);
        }
        if self.pending.is_empty() {
            self.refill();
        }
        Some(self.pending.pop_front().unwrap_or(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::{ChunkDecoder, Conceal, JitterStage, Mixer, MixerInput};
    use super::{DEFAULT_OUT_FRAMES, PLC_FADE_SAMPLES};
    use crate::audio::AudioFormat;
    use crate::audio::chunk::AudioChunk;
    use crate::audio::codec::{AudioCodec, AudioDecoder, EncodedChunk};
    use crate::audio::sample::{Resampler, SyncSampleSource};
    use crate::audio::transform::{ChunkTransform, Gain};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TagDecoder {
        frames: usize,
    }

    impl AudioDecoder for TagDecoder {
        fn decode(&mut self, frame: &EncodedChunk) -> Option<AudioChunk> {
            let channels = frame.format.channel_count as usize;
            let value = frame.sequence_number as f32 * 0.1;
            Some(AudioChunk::new(
                frame.sequence_number,
                frame.format.clone(),
                vec![value; self.frames * channels],
            ))
        }
    }

    struct ScriptedResampler {
        buffer: VecDeque<f32>,
    }

    impl Resampler for ScriptedResampler {
        fn push_sample(&mut self, sample: f32) {
            self.buffer.push_back(sample);
        }

        fn pop_sample(&mut self) -> Option<f32> {
            self.buffer.pop_front()
        }

        fn buffered(&self) -> usize {
            self.buffer.len()
        }
    }

    fn rebuild(_: &AudioFormat) -> Option<TagDecoder> {
        Some(TagDecoder { frames: 480 })
    }

    fn script() -> ScriptedResampler {
        ScriptedResampler {
            buffer: VecDeque::new(),
        }
    }

    fn out_format() -> AudioFormat {
        AudioFormat::new(2, 48000)
    }

    fn frame(sequence_number: u128) -> EncodedChunk {
        EncodedChunk {
            sequence_number,
            codec: AudioCodec::Raw,
            payload: Vec::new(),
            format: AudioFormat::new(2, 48000),
        }
    }

    fn push<D, T, R, M, FD>(mixer: &mut Mixer<D, T, R, M, FD>, id: u32, frame: EncodedChunk)
    where
        D: AudioDecoder,
        T: ChunkTransform,
        R: Resampler,
        M: ChunkTransform,
        FD: FnMut(&AudioFormat) -> Option<D> + Send,
    {
        mixer
            .input_mut(id)
            .expect("subscribed slot")
            .push_frame(frame);
    }

    fn push_via_trait(input: &mut impl MixerInput, frame: EncodedChunk) {
        input.push_frame(frame);
    }

    fn pull<D, T, R, M, FD>(mixer: &mut Mixer<D, T, R, M, FD>, count: usize) -> Vec<f32>
    where
        D: AudioDecoder,
        T: ChunkTransform,
        R: Resampler,
        M: ChunkTransform,
        FD: FnMut(&AudioFormat) -> Option<D> + Send,
    {
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(mixer.next_sync().expect("mixer never ends"));
        }
        out
    }

    #[test]
    fn pushes_flow_to_output_in_order() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(0));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 0.0));
        assert_eq!(mixer.metrics_snapshot().fills, 1);
        push(&mut mixer, id, frame(1));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| (*sample - 0.1).abs() < 1e-6));
        assert_eq!(mixer.metrics_snapshot().fills, 2);
    }

    #[test]
    fn slot_implements_mixer_input() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        push_via_trait(mixer.input_mut(id).expect("subscribed slot"), frame(2));
        assert!(mixer.input_mut(99).is_none());
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| (*sample - 0.2).abs() < 1e-6));
    }

    #[test]
    fn buffered_future_chunk_releases_on_next_push() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(0));
        push(&mut mixer, id, frame(2));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 0.0));
        assert_eq!(mixer.metrics_snapshot().gap_detected, 1);
        push(&mut mixer, id, frame(3));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 0.0));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| (*sample - 0.2).abs() < 1e-6));
    }

    #[test]
    fn refill_releases_only_what_the_block_needs() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(0));
        push(&mut mixer, id, frame(1));
        let _ = pull(&mut mixer, 100);
        let slot = mixer.input_mut(id).expect("slot");
        assert_eq!(slot.jitter.buffer.len(), 1);
    }

    #[test]
    fn duplicate_seq_counts_late_drop() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(0));
        let _ = pull(&mut mixer, 960);
        push(&mut mixer, id, frame(0));
        assert_eq!(mixer.metrics_snapshot().late_dropped, 1);
    }

    #[test]
    fn virgin_nonzero_seq_counts_gap_but_plays() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(5));
        assert_eq!(mixer.metrics_snapshot().gap_detected, 1);
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| (*sample - 0.5).abs() < 1e-6));
    }

    #[test]
    fn starvation_holds_last_sample_then_fades() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(1));
        let _ = pull(&mut mixer, 960);
        let held = pull(&mut mixer, 1);
        assert!((held[0] - 0.1).abs() < 1e-6);
        let faded = pull(&mut mixer, 4);
        assert!(faded[0] < 0.1 && faded[0] > 0.0);
        assert!(faded.windows(2).all(|pair| pair[1] < pair[0]));
        let snapshot = mixer.metrics_snapshot();
        assert_eq!(snapshot.underrun, 1);
        assert_eq!(snapshot.plc_hold, 960);
    }

    #[test]
    fn conceal_run_covers_full_fade_without_pulling() {
        let mut conceal = Conceal::new();
        assert_eq!(conceal.next(Some(0.5)), 0.5);
        assert_eq!(conceal.next(None), 0.5);
        for _ in 1..PLC_FADE_SAMPLES {
            conceal.next(None);
        }
        assert_eq!(conceal.underrun, 1);
        assert_eq!(conceal.plc_hold, PLC_FADE_SAMPLES);
        assert_eq!(conceal.next(Some(0.25)), 0.25);
    }

    #[test]
    fn two_peers_sum_and_clip() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let first = mixer.subscribe((), rebuild, script());
        let second = mixer.subscribe((), rebuild, script());
        push(&mut mixer, first, frame(4));
        push(&mut mixer, second, frame(4));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 0.8));
        mixer.unsubscribe(first);
        mixer.unsubscribe(second);
        let first = mixer.subscribe((), rebuild, script());
        let second = mixer.subscribe((), rebuild, script());
        push(&mut mixer, first, frame(9));
        push(&mut mixer, second, frame(9));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 1.0));
        assert_eq!(mixer.metrics_snapshot().clip_hits, 960);
    }

    #[test]
    fn per_peer_transform_selects_processing() {
        let (gain, _) = Gain::shared(0, 500);
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe(gain, rebuild, script());
        push(&mut mixer, id, frame(5));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn bus_transform_applies_to_mix() {
        let (gain, _) = Gain::shared(0, 500);
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, gain);
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(5));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn unsubscribe_stops_peer() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        assert_eq!(mixer.peer_count(), 1);
        mixer.unsubscribe(id);
        assert_eq!(mixer.peer_count(), 0);
        let out = pull(&mut mixer, 4);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn decoder_rebuilds_once_per_format() {
        let rebuilds = AtomicUsize::new(0);
        let mut decoder = ChunkDecoder::new(|_: &AudioFormat| {
            rebuilds.fetch_add(1, Ordering::Relaxed);
            Some(TagDecoder { frames: 480 })
        });
        let mono = AudioFormat::new(1, 48000);
        let stereo = AudioFormat::new(2, 48000);
        let mono_frame = EncodedChunk {
            sequence_number: 0,
            codec: AudioCodec::Raw,
            payload: Vec::new(),
            format: mono,
        };
        let stereo_frame = EncodedChunk {
            sequence_number: 1,
            codec: AudioCodec::Raw,
            payload: Vec::new(),
            format: stereo,
        };
        assert!(decoder.decode_frame(&mono_frame).is_some());
        assert!(decoder.decode_frame(&mono_frame).is_some());
        assert!(decoder.decode_frame(&stereo_frame).is_some());
        assert_eq!(rebuilds.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn decoder_failure_skips_frame() {
        let mut decoder = ChunkDecoder::new(|_: &AudioFormat| None::<TagDecoder>);
        assert!(decoder.decode_frame(&frame(0)).is_none());
    }

    #[test]
    fn jitter_stage_buffers_future_and_releases() {
        let mut stage = JitterStage::new(10);
        let format = AudioFormat::new(2, 48000);
        let chunk = |sequence_number: u128| {
            AudioChunk::new(
                sequence_number,
                format.clone(),
                vec![sequence_number as f32; 4],
            )
        };
        stage.push(chunk(0));
        stage.push(chunk(2));
        assert_eq!(stage.pull().map(|c| c.sequence_number), Some(0));
        assert_eq!(stage.pull().map(|c| c.sequence_number), None);
        stage.push(chunk(3));
        let released = stage.pull().expect("buffered chunk releases");
        assert_eq!(released.sequence_number, 2);
        assert_eq!(stage.gap_detected, 1);
        assert_eq!(stage.late_dropped, 0);
    }

    #[test]
    fn input_slot_runs_pipeline_on_push() {
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, ());
        let id = mixer.subscribe((), rebuild, script());
        let slot = mixer.input_mut(id).expect("slot");
        slot.push_frame(frame(3));
        let ordered = slot.jitter.pull().expect("ordered chunk");
        assert_eq!(ordered.sequence_number, 3);
        for sample in ordered.audio_data {
            slot.resampler.push_sample(sample);
        }
        assert_eq!(slot.resampler.pop_sample(), Some(0.3));
    }

    #[test]
    fn bus_gain_overshoot_caught_by_terminal_clip() {
        let (gain, _) = Gain::shared(200, 500);
        let mut mixer = Mixer::new(out_format(), 10, DEFAULT_OUT_FRAMES, gain);
        let id = mixer.subscribe((), rebuild, script());
        push(&mut mixer, id, frame(9));
        let out = pull(&mut mixer, 960);
        assert!(out.iter().all(|sample| *sample == 1.0));
        assert_eq!(mixer.metrics_snapshot().clip_hits, 960);
    }
}
