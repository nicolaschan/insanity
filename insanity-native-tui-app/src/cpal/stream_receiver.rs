use anyhow::anyhow;
use cpal::{
    Device, Sample, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use insanity_core::audio::{AudioFormat, sample::SampleSource};

use crate::audio::get_input_config;

pub struct CpalStreamReceiver {
    _stream: send_safe::SendWrapperThread<Option<Stream>>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<f32>,
    sample_rate: u32,
    channels: u16,
}

impl SampleSource for CpalStreamReceiver {
    async fn next(&mut self) -> Option<f32> {
        self.receiver.recv().await
    }
    fn format(&self) -> AudioFormat {
        AudioFormat::new(self.channels, self.sample_rate)
    }
}

impl TryFrom<Device> for CpalStreamReceiver {
    type Error = anyhow::Error;

    fn try_from(value: Device) -> Result<Self, Self::Error> {
        match make_single_input(value) {
            Ok(s) => Ok(s),
            Err(e) => {
                log::warn!("{}", e);
                Err(e)
            }
        }
    }
}

pub fn make_single_input(device: Device) -> Result<CpalStreamReceiver, anyhow::Error> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let Ok((fmt, cfg)) = get_input_config(&device) else {
        return Err(anyhow!(
            "Failed to get input config falling back to silence"
        ));
    };
    let cfg2 = cfg.clone();
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        match setup_input_stream(&fmt, &cfg2, &device, tx) {
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
    Ok(CpalStreamReceiver {
        _stream: wrapper,
        receiver: rx,
        sample_rate: cfg.sample_rate.0,
        channels: cfg.channels,
    })
}

fn setup_input_stream(
    sample_format: &SampleFormat,
    config: &StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<f32>,
) -> anyhow::Result<Stream> {
    match sample_format {
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
    }
}

fn run_input<T: Sample>(
    config: &StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<f32>,
) -> anyhow::Result<Stream> {
    let err_fn = |err| eprintln!("input stream error: {err}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for s in data.iter() {
                    let _ = sender.send(s.to_f32());
                }
            },
            err_fn,
        )
        .map_err(|e| anyhow::anyhow!("build input stream: {e}"))
}
