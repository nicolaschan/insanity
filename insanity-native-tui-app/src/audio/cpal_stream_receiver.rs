use std::iter::ExactSizeIterator;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use cpal::{
    Device, FromSample, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
    traits::{DeviceTrait, StreamTrait},
};

use super::config::{STREAM_BUILD_TIMEOUT, ordered_input_configs};
use super::cpal_stream::sample_format_dispatch;
use super::mixer::RING_CAPACITY_BLOCKS;
use super::stream_errors::FatalReporter;
use insanity_core::audio::AudioFormat;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::sample::SampleSource;

#[derive(Default)]
pub(crate) struct InputStats {
    overruns: AtomicUsize,
    data_callbacks: AtomicU64,
}

impl InputStats {
    fn note_overrun(&self, samples: usize) {
        self.overruns.fetch_add(samples, Ordering::Relaxed);
    }

    fn note_data_callback(&self) {
        self.data_callbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn data_callbacks(&self) -> u64 {
        self.data_callbacks.load(Ordering::Relaxed)
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
    _stream: send_safe::SendWrapperThread<Result<Stream, String>>,
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
    let written = (writable > 0)
        .then(|| producer.inner.write_chunk_uninit(writable).ok())
        .flatten()
        .map_or(0, |chunk| chunk.fill_from_iter(&mut iter));
    let dropped = len.saturating_sub(written);
    if dropped > 0 {
        stats.note_overrun(dropped);
    }
    if written > 0 {
        producer.wake.notify_one();
    }
}

fn try_input_candidate(
    device: &Device,
    audio_config: &AudioPipelineConfig,
    reporter: &FatalReporter,
    stats: &Arc<InputStats>,
    wake: &Arc<tokio::sync::Notify>,
    config: SupportedStreamConfig,
) -> Result<CpalStreamReceiver, anyhow::Error> {
    let sample_format = config.sample_format();
    let channels = config.channels();
    let sample_rate = config.sample_rate();
    let format = AudioFormat::new(channels, sample_rate);
    let block_samples = channels as usize * audio_config.frames();
    anyhow::ensure!(block_samples > 0, "invalid block size");
    let (producer, consumer) = rtrb::RingBuffer::new(block_samples * RING_CAPACITY_BLOCKS);
    let build_wake = Arc::clone(wake);
    let build_stats = Arc::clone(stats);
    let build_reporter = reporter.clone();
    let attempt_device = device.clone();
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        setup_input_stream(
            config,
            &attempt_device,
            producer,
            build_wake,
            build_stats,
            build_reporter,
        )
        .map_err(|e| format!("{e:#}"))
    });
    if let Some(reason) = wrapper
        .execute(|slot| match slot {
            Ok(stream) if stream.play().is_ok() => None,
            Ok(_) => Some("play() failed".to_string()),
            Err(e) => Some(std::mem::take(e)),
        })
        .unwrap_or_else(|_| Some("audio thread died".to_string()))
    {
        log::trace!("{sample_format:?} {channels}ch {sample_rate}Hz: {reason}");
        anyhow::bail!("{sample_format:?} {channels}ch {sample_rate}Hz: {reason}");
    }
    Ok(CpalStreamReceiver {
        _stream: wrapper,
        consumer,
        wake: Arc::clone(wake),
        format,
    })
}

pub(crate) fn make_single_input(
    device: Device,
    audio_config: &AudioPipelineConfig,
    reporter: FatalReporter,
    stats: &Arc<InputStats>,
) -> Result<CpalStreamReceiver, anyhow::Error> {
    let candidates = ordered_input_configs(&device, audio_config);
    log::debug!("Trying {} ranked input configs", candidates.len());
    log::trace!("Candidate configs: {candidates:?}");
    let wake = Arc::new(tokio::sync::Notify::new());
    let mut failures = Vec::new();
    for candidate in candidates {
        match try_input_candidate(&device, audio_config, &reporter, stats, &wake, candidate) {
            Ok(receiver) => {
                log::info!(
                    "Selected input stream: {}ch {}Hz {}",
                    candidate.channels(),
                    candidate.sample_rate(),
                    candidate.sample_format()
                );
                return Ok(receiver);
            }
            Err(e) => failures.push(format!("{e:#}")),
        }
    }
    anyhow::bail!(
        "Failed to start input stream (tried {} configs: {}), parking input",
        failures.len(),
        failures.join("; ")
    );
}

fn setup_input_stream(
    config: SupportedStreamConfig,
    device: &Device,
    producer: rtrb::Producer<f32>,
    wake: Arc<tokio::sync::Notify>,
    stats: Arc<InputStats>,
    reporter: FatalReporter,
) -> anyhow::Result<Stream> {
    let producer = NotifyingProducer::new(producer, wake);
    sample_format_dispatch!(
        config.sample_format(),
        run_input,
        config.config(),
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
                stats.note_data_callback();
                publish_samples(
                    &mut producer,
                    &stats,
                    data.iter().map(|s| s.to_sample::<f32>()),
                );
            },
            err_fn,
            Some(STREAM_BUILD_TIMEOUT),
        )
        .map_err(|e| anyhow::anyhow!("build input stream: {e}"))
}
