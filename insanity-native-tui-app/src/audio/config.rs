use std::time::Duration;

use anyhow::Context;
use cpal::{
    BufferSize, Device, SampleFormat, StreamConfig, SupportedStreamConfig, traits::DeviceTrait,
};

pub(crate) const STREAM_BUILD_TIMEOUT: Duration = Duration::from_secs(5);

fn to_stream_pair(supported: SupportedStreamConfig) -> (SampleFormat, StreamConfig) {
    let fmt = supported.sample_format();
    let cfg = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: BufferSize::Default,
    };
    (fmt, cfg)
}

pub(crate) fn get_input_config(device: &Device) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let supported = device
        .default_input_config()
        .context("Failed to get default input config")?;
    Ok(to_stream_pair(supported))
}

pub(crate) fn get_output_config(device: &Device) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let supported = device
        .default_output_config()
        .context("Failed to get default output config")?;
    Ok(to_stream_pair(supported))
}
