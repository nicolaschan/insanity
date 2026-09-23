#![allow(dead_code)]
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::codec::{AudioCodec, AudioDecoder, EncodedChunk};
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::mixer::{Mixer, SlotId};
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::{ChunkTransform, Gain, GainControl};
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::mixer::{
    MAX_VOLUME, PeerChain, PeerControls, chain_from_controls,
};
use std::sync::Arc;

pub struct PassthroughEncoder;

impl ChunkTransform for PassthroughEncoder {
    type OutputT = Option<EncodedChunk>;

    fn transform(&mut self, chunk: AudioChunk) -> Option<EncodedChunk> {
        let mut payload = Vec::with_capacity(chunk.audio_data.len() * 4);
        payload.extend(
            chunk
                .audio_data
                .iter()
                .flat_map(|sample| sample.to_le_bytes()),
        );
        Some(EncodedChunk {
            sequence_number: chunk.sequence_number,
            codec: AudioCodec::Raw,
            payload,
            format: chunk.format,
        })
    }
}

pub struct PassthroughDecoder;

impl AudioDecoder for PassthroughDecoder {
    fn decode(&mut self, frame: &EncodedChunk) -> Option<AudioChunk> {
        if frame.codec != AudioCodec::Raw || !frame.payload.len().is_multiple_of(4) {
            return None;
        }
        Some(AudioChunk::new(
            frame.sequence_number,
            frame.format.clone(),
            frame
                .payload
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect(),
        ))
    }
}

pub type UnitMixer =
    Mixer<PassthroughDecoder, PeerChain, Gain, fn(&AudioFormat) -> Option<PassthroughDecoder>>;

pub fn rebuild_passthrough(_: &AudioFormat) -> Option<PassthroughDecoder> {
    Some(PassthroughDecoder)
}

pub fn unit_mixer(bus_volume: usize) -> (UnitMixer, Arc<GainControl>) {
    unit_mixer_with_jitter(bus_volume, AudioPipelineConfig::default().jitter_chunks())
}

pub fn unit_mixer_with_jitter(
    bus_volume: usize,
    jitter_chunks: usize,
) -> (UnitMixer, Arc<GainControl>) {
    let audio_config = AudioPipelineConfig::default()
        .with_jitter_chunks(jitter_chunks)
        .expect("test jitter valid");
    let (bus, bus_control) = Gain::shared(bus_volume, MAX_VOLUME);
    let mixer = Mixer::new(
        AudioFormat::new(audio_config.channels(), audio_config.sample_rate()),
        audio_config,
        bus,
    );
    (mixer, bus_control)
}

pub fn add_unit_peer(mixer: &mut UnitMixer, volume: usize, denoise: DenoiseSelection) -> SlotId {
    let controls = PeerControls::new(volume, denoise);
    let chain = chain_from_controls(&controls);
    mixer.subscribe(chain, rebuild_passthrough)
}

pub fn push_chunk(mixer: &mut UnitMixer, slot: SlotId, chunk: AudioChunk) {
    let mut encoder = PassthroughEncoder;
    let frame = encoder.transform(chunk).expect("encode");
    mixer.push_to_slot(slot, frame);
}

pub fn push_value(mixer: &mut UnitMixer, slot: SlotId, sequence: u128, value: f32) {
    let audio_config = AudioPipelineConfig::default();
    push_chunk(
        mixer,
        slot,
        AudioChunk::new(
            sequence,
            AudioFormat::new(2, audio_config.sample_rate()),
            vec![value; audio_config.block_samples()],
        ),
    );
}

pub fn render(mixer: &mut UnitMixer, count: usize) -> Vec<f32> {
    (0..count)
        .map(|_| mixer.next_sync().unwrap_or(0.0))
        .collect()
}

pub fn mixer_with_capacity(chunks: usize) -> UnitMixer {
    unit_mixer_with_jitter(100, chunks).0
}

pub fn add_peer(mixer: &mut UnitMixer) -> SlotId {
    add_unit_peer(mixer, 100, DenoiseSelection::None)
}

pub fn feed(mixer: &mut UnitMixer, id: SlotId, first_seq: u128, count: usize, value: f32) {
    for seq in first_seq..first_seq + count as u128 {
        push_value(mixer, id, seq, value);
    }
}

pub fn fill(mixer: &mut UnitMixer, samples: usize) -> Vec<f32> {
    render(mixer, samples)
}

pub fn underruns(mixer: &UnitMixer) -> usize {
    mixer.metrics_snapshot().underrun
}

pub fn plc_hold(mixer: &UnitMixer) -> usize {
    mixer.metrics_snapshot().plc_hold
}

pub fn assert_all_finite(samples: &[f32]) {
    assert!(samples.iter().all(|s| s.is_finite()));
}
