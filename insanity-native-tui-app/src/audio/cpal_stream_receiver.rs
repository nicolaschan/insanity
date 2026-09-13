use anyhow::anyhow;
use cpal::{
    Device, FromSample, SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures_util::stream;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::sample::AudioStream;
use insanity_core::audio::{AudioFormat, device::UNKNOWN_DEVICE_NAME};

use super::config::get_input_config;

pub struct CpalInput {
    pub name: String,
    pub samples: AudioStream,
}

impl CpalInput {
    pub fn default(audio_config: AudioPipelineConfig) -> anyhow::Result<Self> {
        let device = cpal::default_host().default_input_device();
        match device {
            Some(device) => make_single_input(device, audio_config),
            None => Err(anyhow!("No default device available")),
        }
    }
}

pub fn make_single_input(
    device: Device,
    audio_config: AudioPipelineConfig,
) -> anyhow::Result<CpalInput> {
    let name = device_name(&device);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let Ok((fmt, cfg)) = get_input_config(&device, audio_config) else {
        return Err(anyhow!(
            "Failed to get input config falling back to silence"
        ));
    };
    let format = AudioFormat::new(cfg.channels, cfg.sample_rate);
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        match setup_input_stream(fmt, cfg, &device, tx) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("Failed to build input stream, falling back to silence: {e:?}");
                None
            }
        }
    });
    let play_ok = wrapper
        .execute(|s| match s {
            Some(stream) => stream.play().is_ok(),
            None => false,
        })
        .unwrap_or(false);
    if !play_ok {
        return Err(anyhow!(
            "Failed to start input stream, falling back to silence"
        ));
    }
    let samples = stream::unfold((rx, wrapper), |(mut rx, wrapper)| async move {
        let sample = rx.recv().await?;
        Some((sample, (rx, wrapper)))
    });
    Ok(CpalInput {
        name,
        samples: AudioStream::new(format, samples),
    })
}

fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or(UNKNOWN_DEVICE_NAME.into())
}

fn setup_input_stream(
    sample_format: SampleFormat,
    config: StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<f32>,
) -> anyhow::Result<cpal::Stream> {
    match sample_format {
        SampleFormat::I8 => run_input::<i8>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::I32 => run_input::<i32>(config, device, sender),
        SampleFormat::I64 => run_input::<i64>(config, device, sender),
        SampleFormat::U8 => run_input::<u8>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
        SampleFormat::U32 => run_input::<u32>(config, device, sender),
        SampleFormat::U64 => run_input::<u64>(config, device, sender),
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::F64 => run_input::<f64>(config, device, sender),
        other => Err(anyhow!("unsupported input sample format {other:?}")),
    }
}

fn run_input<T>(
    config: StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<f32>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let err_fn = |err| eprintln!("input stream error: {err}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for s in data.iter() {
                    let _ = sender.send(s.to_sample::<f32>());
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow::anyhow!("build input stream: {e}"))
}
