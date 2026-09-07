use anyhow::anyhow;
use cpal::{
    Device, Sample, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use insanity_core::audio::{
    AudioFormat,
    chunk::{AudioChunk, ChunkSource},
};

use crate::audio::get_input_config;

pub struct CpalStreamReceiver {
    _stream: send_safe::SendWrapperThread<Option<Stream>>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<Vec<f32>>,
    format: AudioFormat,
    next_sequence: u128,
}

impl CpalStreamReceiver {
    pub fn format(&self) -> AudioFormat {
        self.format.clone()
    }
}

impl ChunkSource for CpalStreamReceiver {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        let audio_data = self.receiver.recv().await?;
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(
            sequence_number,
            self.format.clone(),
            audio_data,
        ))
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
        format: AudioFormat::new(cfg.channels, cfg.sample_rate.0),
        next_sequence: 0,
    })
}

fn setup_input_stream(
    sample_format: &SampleFormat,
    config: &StreamConfig,
    device: &Device,
    sender: tokio::sync::mpsc::UnboundedSender<Vec<f32>>,
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
    sender: tokio::sync::mpsc::UnboundedSender<Vec<f32>>,
) -> anyhow::Result<Stream> {
    let err_fn = |err| eprintln!("input stream error: {err}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let _ = sender.send(data.iter().map(Sample::to_f32).collect());
            },
            err_fn,
        )
        .map_err(|e| anyhow::anyhow!("build input stream: {e}"))
}
