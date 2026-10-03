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
use super::cpal_stream::sample_format_dispatch;
use super::output::RING_CAPACITY_BLOCKS;
use super::stream_errors::FatalReporter;

#[derive(Default)]
pub(crate) struct InputStats {
    overruns: AtomicUsize,
}

impl InputStats {
    fn note_overrun(&self, samples: usize) {
        self.overruns.fetch_add(samples, Ordering::Relaxed);
    }

    pub(crate) fn overruns(&self) -> usize {
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

pub(crate) struct CpalStreamReceiver {
    _stream: send_safe::SendWrapperThread<Option<Stream>>,
    consumer: rtrb::Consumer<f32>,
    wake: Arc<tokio::sync::Notify>,
    format: AudioFormat,
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

pub(crate) fn make_single_input(
    device: Device,
    audio_config: &AudioPipelineConfig,
    reporter: FatalReporter,
    stats: &Arc<InputStats>,
) -> Result<CpalStreamReceiver, anyhow::Error> {
    let Ok((fmt, cfg)) = get_input_config(&device, audio_config) else {
        return Err(anyhow!(
            "Failed to get input config, parking input until a device appears"
        ));
    };
    let format = AudioFormat::new(cfg.channels, cfg.sample_rate);
    let block_samples = cfg.channels as usize * audio_config.frames();
    assert!(block_samples > 0);
    let (producer, consumer) = rtrb::RingBuffer::new(block_samples * RING_CAPACITY_BLOCKS);
    let wake = Arc::new(tokio::sync::Notify::new());
    let build_wake = Arc::clone(&wake);
    let build_stats = Arc::clone(stats);
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        match setup_input_stream(
            fmt,
            cfg,
            &device,
            producer,
            build_wake,
            build_stats,
            reporter,
        ) {
            Ok(stream) => Some(stream),
            Err(e) => {
                log::warn!("Failed to build input stream, parking input: {e:?}");
                None
            }
        }
    });
    let playing = wrapper
        .execute(|stream| stream.as_ref().is_some_and(|active| active.play().is_ok()))
        .unwrap_or(false);
    if !playing {
        return Err(anyhow!(
            "Failed to start input stream, parking input until a device appears"
        ));
    }
    Ok(CpalStreamReceiver {
        _stream: wrapper,
        consumer,
        wake,
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
    reporter: FatalReporter,
) -> anyhow::Result<Stream> {
    let producer = NotifyingProducer::new(producer, wake);
    sample_format_dispatch!(
        sample_format,
        run_input,
        config,
        device,
        producer,
        stats,
        reporter
    )
}

fn run_input<T>(
    config: StreamConfig,
    device: &Device,
    mut producer: NotifyingProducer,
    stats: Arc<InputStats>,
    reporter: FatalReporter,
) -> anyhow::Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let err_fn = move |err: cpal::Error| {
        reporter.report_input(err.kind());
    };
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
