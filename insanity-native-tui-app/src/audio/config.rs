use std::time::Duration;

use anyhow::Context;
use cpal::{
    BufferSize, Device, SampleFormat, StreamConfig, SupportedStreamConfig, traits::DeviceTrait,
};

use insanity_core::audio::config::AudioPipelineConfig;

pub(crate) const STREAM_BUILD_TIMEOUT: Duration = Duration::from_secs(5);

const PULSEAUDIO_HOST: &str = "PulseAudio";

fn host_name() -> &'static str {
    super::cpal_registry::shared_host().id().name()
}

fn callback_buffer_size(
    host: &str,
    supported: &cpal::SupportedBufferSize,
    frames: u32,
) -> BufferSize {
    match (host, supported) {
        (PULSEAUDIO_HOST, cpal::SupportedBufferSize::Range { min, max }) => {
            BufferSize::Fixed(frames.clamp(*min, *max))
        }
        _ => BufferSize::Default,
    }
}

fn to_stream_pair(supported: SupportedStreamConfig, frames: u32) -> (SampleFormat, StreamConfig) {
    let fmt = supported.sample_format();
    let cfg = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: callback_buffer_size(host_name(), supported.buffer_size(), frames),
    };
    (fmt, cfg)
}

pub(crate) fn get_input_config(
    device: &Device,
    audio_config: &AudioPipelineConfig,
) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let supported = device
        .default_input_config()
        .context("Failed to get default input config")?;
    Ok(to_stream_pair(supported, audio_config.frames() as u32))
}

pub(crate) fn get_output_config(
    device: &Device,
    audio_config: &AudioPipelineConfig,
) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let supported = device
        .default_output_config()
        .context("Failed to get default output config")?;
    Ok(to_stream_pair(supported, audio_config.frames() as u32))
}

#[cfg(test)]
mod tests {
    use cpal::{BufferSize, SupportedBufferSize};
    use insanity_core::audio::config::AudioPipelineConfig;

    use super::callback_buffer_size;

    #[test]
    fn pulseaudio_pins_callback_frames_otherwise_default() {
        let frames = AudioPipelineConfig::default().frames();
        let frames_u32 = u32::try_from(frames).unwrap();
        let wide = SupportedBufferSize::Range {
            min: 1,
            max: 1 << 20,
        };
        assert_eq!(
            callback_buffer_size("PulseAudio", &wide, frames_u32),
            BufferSize::Fixed(frames_u32)
        );
        assert_eq!(
            callback_buffer_size("PulseAudio", &SupportedBufferSize::Unknown, frames_u32),
            BufferSize::Default
        );
        let range = SupportedBufferSize::Range { min: 480, max: 480 };
        for host in ["WASAPI", "CoreAudio", "ALSA", "PipeWire", "JACK"] {
            assert_eq!(
                callback_buffer_size(host, &range, frames_u32),
                BufferSize::Default,
                "{host}"
            );
        }
    }
}
