use std::iter::ExactSizeIterator;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::anyhow;
use cpal::{
    Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::sample::SampleSource;

use super::config::get_input_config;
use super::output::RING_CAPACITY_BLOCKS;

#[derive(Default)]
pub struct InputStats {
    overruns: AtomicUsize,
}

impl InputStats {
    fn new() -> Self {
        Self {
            overruns: AtomicUsize::new(0),
        }
    }

    fn note_overrun(&self, samples: usize) {
        self.overruns.fetch_add(samples, Ordering::Relaxed);
    }

    pub fn overruns(&self) -> usize {
        self.overruns.load(Ordering::Relaxed)
    }
}

// Combination of rtrb Producer and notifier to ensure notification on drop of producer.
struct NotifyingProducer {
    inner: rtrb::Producer<f32>,
    wake: Arc<tokio::sync::Notify>,
}

impl NotifyingProducer {
    fn new(inner: rtrb::Producer<f32>, wake: Arc<tokio::sync::Notify>) -> Self {
        Self { inner, wake }
    }
}

impl Drop for NotifyingProducer {
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

pub struct CpalStreamReceiver {
    _stream: send_safe::SendWrapperThread<Option<Stream>>,
    consumer: rtrb::Consumer<f32>,
    wake: Arc<tokio::sync::Notify>,
    stats: Arc<InputStats>,
    format: AudioFormat,
}

impl CpalStreamReceiver {
    pub fn stats(&self) -> Arc<InputStats> {
        Arc::clone(&self.stats)
    }
}

impl SampleSource for CpalStreamReceiver {
    fn format(&self) -> &AudioFormat {
        &self.format
    }

    async fn next(&mut self) -> Option<f32> {
        loop {
            if let Ok(sample) = self.consumer.pop() {
                return Some(sample);
            }
            if self.consumer.is_abandoned() {
                return None;
            }
            self.wake.notified().await;
        }
    }
}

fn publish_samples(
    producer: &mut NotifyingProducer,
    stats: &Arc<InputStats>,
    samples: impl ExactSizeIterator<Item = f32>,
) {
    let len = samples.len();
    if len == 0 {
        return;
    }
    let mut iter = samples.into_iter();
    let writable = producer.inner.slots().min(len);
    let written = if writable > 0 {
        match producer.inner.write_chunk_uninit(writable) {
            Ok(chunk) => chunk.fill_from_iter(&mut iter),
            Err(_) => 0,
        }
    } else {
        0
    };
    let dropped = len.saturating_sub(written);
    if dropped > 0 {
        stats.note_overrun(dropped);
    }
    if written > 0 {
        producer.wake.notify_one();
    }
}

pub fn make_single_input(
    device: Device,
    audio_config: AudioPipelineConfig,
) -> Result<CpalStreamReceiver, anyhow::Error> {
    let Ok((fmt, cfg)) = get_input_config(&device, audio_config) else {
        return Err(anyhow!(
            "Failed to get input config falling back to silence"
        ));
    };
    let format = AudioFormat::new(cfg.channels, cfg.sample_rate);
    let block_samples = cfg.channels as usize * audio_config.frames();
    if block_samples == 0 {
        return Err(anyhow!("Invalid input config with zero block size"));
    }
    let (producer, consumer) = rtrb::RingBuffer::new(block_samples * RING_CAPACITY_BLOCKS);
    let wake = Arc::new(tokio::sync::Notify::new());
    let stats = Arc::new(InputStats::new());
    let build_wake = Arc::clone(&wake);
    let build_stats = Arc::clone(&stats);
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        match setup_input_stream(fmt, cfg, &device, producer, build_wake, build_stats) {
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
        consumer,
        wake,
        stats,
        format,
    })
}

fn setup_input_stream(
    sample_format: SampleFormat,
    config: StreamConfig,
    device: &Device,
    producer: rtrb::Producer<f32>,
    wake: Arc<tokio::sync::Notify>,
    stats: Arc<InputStats>,
) -> anyhow::Result<Stream> {
    let producer = NotifyingProducer::new(producer, wake);
    match sample_format {
        SampleFormat::I8 => run_input::<i8>(config, device, producer, stats),
        SampleFormat::I16 => run_input::<i16>(config, device, producer, stats),
        SampleFormat::I32 => run_input::<i32>(config, device, producer, stats),
        SampleFormat::I64 => run_input::<i64>(config, device, producer, stats),
        SampleFormat::U8 => run_input::<u8>(config, device, producer, stats),
        SampleFormat::U16 => run_input::<u16>(config, device, producer, stats),
        SampleFormat::U32 => run_input::<u32>(config, device, producer, stats),
        SampleFormat::U64 => run_input::<u64>(config, device, producer, stats),
        SampleFormat::F32 => run_input::<f32>(config, device, producer, stats),
        SampleFormat::F64 => run_input::<f64>(config, device, producer, stats),
        other => Err(anyhow!("unsupported input sample format {other:?}")),
    }
}

fn run_input<T>(
    config: StreamConfig,
    device: &Device,
    mut producer: NotifyingProducer,
    stats: Arc<InputStats>,
) -> anyhow::Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let err_fn = |err: cpal::Error| super::stream_errors::note_input_error(err.kind());
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                publish_samples(
                    &mut producer,
                    &stats,
                    data.iter().map(|s| s.to_sample::<f32>()),
                );
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow::anyhow!("build input stream: {e}"))
}
