use anyhow::anyhow;
use cpal::{
    Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::sample::SampleSource;

use super::config::get_input_config;

pub struct CpalStreamReceiver {
    _stream: send_safe::SendWrapperThread<Option<Stream>>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<f32>,
}

impl SampleSource for CpalStreamReceiver {
    async fn next(&mut self) -> Option<f32> {
        self.receiver.recv().await
    }
}

pub fn make_single_input(
    device: Device,
) -> Result<(CpalStreamReceiver, AudioFormat), anyhow::Error> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let Ok((fmt, cfg)) = get_input_config(&device) else {
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
    Ok((
        CpalStreamReceiver {
            _stream: wrapper,
            receiver: rx,
        },
        format,
    ))
}

fn setup_input_stream(
    sample_format: SampleFormat,
    config: StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<f32>,
) -> anyhow::Result<Stream> {
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
) -> anyhow::Result<Stream>
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
