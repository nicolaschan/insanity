#![allow(dead_code)]
use futures_util::stream;
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::codec::EncodedChunk;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::mixer::Mixer;
use insanity_core::audio::transform::Gain;
use insanity_native_tui_app::audio::hub::AudioInputHub;
use insanity_native_tui_app::audio::mixer::{AppMixer, MAX_VOLUME};
use opus::{Channels, Decoder};

pub struct SineSource {
    phase: f32,
    format: AudioFormat,
    freq: f32,
    amp: f32,
}

impl SineSource {
    pub fn new(sr: u32, freq: f32) -> Self {
        Self::new_amp(sr, freq, 0.5)
    }

    pub fn new_amp(sr: u32, freq: f32, amp: f32) -> Self {
        Self {
            phase: 0.0,
            format: AudioFormat::new(2, sr),
            freq,
            amp,
        }
    }

    fn step(&mut self) -> f32 {
        let v = (self.phase * 2.0 * std::f32::consts::PI).sin() * self.amp;
        self.phase = (self.phase + self.freq / self.format.sample_rate as f32) % 1.0;
        v
    }

    pub fn reference(freq: f32, len: usize) -> Vec<f32> {
        let mut src = Self::new_amp(48000, freq, 0.5);
        (0..len).map(|_| src.step()).collect()
    }
}

impl Iterator for SineSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

pub fn hub_from_source<S>(source: S, format: AudioFormat) -> AudioInputHub
where
    S: Iterator<Item = f32> + Send + 'static,
{
    AudioInputHub::new(stream::iter(source), format, AudioPipelineConfig::default())
}

pub fn new_no_device_mixer() -> AppMixer {
    let audio_config = AudioPipelineConfig::default();
    let (bus, _) = Gain::shared(100, MAX_VOLUME);
    Mixer::new(
        AudioFormat::new(audio_config.channels(), audio_config.sample_rate()),
        audio_config,
        bus,
    )
}

pub fn decode_frame_to_chunk(decoder: &mut Decoder, frame: &EncodedChunk) -> Option<AudioChunk> {
    let channels = frame.format.channel_count;
    let Ok(nb) = decoder.get_nb_samples(&frame.payload[..]) else {
        return None;
    };
    let len = nb * (channels as usize);
    let mut buf = vec![0f32; len];
    if decoder
        .decode_float(&frame.payload[..], &mut buf[..], false)
        .is_err()
    {
        return None;
    }
    Some(AudioChunk::new(
        frame.sequence_number,
        AudioFormat::new(channels, frame.format.sample_rate),
        buf,
    ))
}

pub fn opus_channels(channel_count: u16) -> Channels {
    match channel_count {
        1 => Channels::Mono,
        _ => Channels::Stereo,
    }
}
