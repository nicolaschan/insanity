use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::mixer::{DEFAULT_JITTER_CHUNKS, DEFAULT_OUT_FRAMES, Mixer};
use insanity_core::audio::transform::Gain;
use rtrb::{Consumer, RingBuffer};
use tokio::sync::mpsc;

use super::config::get_output_config;
use super::mixer::{MIXER_OPS_BOUND, MixerClient, run_mixer_owner};
use super::params::{CHANNELS, MAX_VOLUME, SAMPLE_RATE};

// Output mixer

pub struct FillStats {
    total_nanos: AtomicU64,
    fills: AtomicUsize,
}

impl FillStats {
    pub fn new() -> Self {
        FillStats {
            total_nanos: AtomicU64::new(0),
            fills: AtomicUsize::new(0),
        }
    }

    pub fn record(&self, elapsed: std::time::Duration) {
        self.fills.fetch_add(1, Ordering::Relaxed);
        self.total_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn avg_nanos(&self) -> u64 {
        let fills = self.fills.load(Ordering::Relaxed) as u64;
        if fills == 0 {
            return 0;
        }
        self.total_nanos.load(Ordering::Relaxed) / fills
    }
}

impl Default for FillStats {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) const RING_CAPACITY_BLOCKS: usize = 8;

pub(crate) struct OutputStats {
    underruns: AtomicUsize,
    overruns: AtomicUsize,
}

impl OutputStats {
    fn new() -> Self {
        OutputStats {
            underruns: AtomicUsize::new(0),
            overruns: AtomicUsize::new(0),
        }
    }

    fn note_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_overrun(&self, samples: usize) {
        self.overruns.fetch_add(samples, Ordering::Relaxed);
    }

    pub(crate) fn underruns(&self) -> usize {
        self.underruns.load(Ordering::Relaxed)
    }

    pub(crate) fn overruns(&self) -> usize {
        self.overruns.load(Ordering::Relaxed)
    }
}

impl Default for OutputStats {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub(crate) struct OutputHandle {
    pub(crate) client: MixerClient,
    pub(crate) timing: Arc<FillStats>,
    pub(crate) format: AudioFormat,
    pub(crate) stats: Arc<OutputStats>,
}

pub(crate) struct OutputGuard {
    _stream: Option<send_safe::SendWrapperThread<Option<Stream>>>,
}

pub(crate) struct AudioOutput {
    pub(crate) handle: OutputHandle,
    pub(crate) _guard: OutputGuard,
}

pub(crate) fn start_output() -> AudioOutput {
    let host = cpal::default_host();
    let output = host
        .default_output_device()
        .and_then(|device| match get_output_config(&device) {
            Ok((sample_format, config)) => Some((device, sample_format, config)),
            Err(e) => {
                log::warn!("Failed to get output config, falling back to dummy: {e}");
                None
            }
        });
    let format = output
        .as_ref()
        .map(|(_, _, config)| AudioFormat::new(config.channels, config.sample_rate))
        .unwrap_or(AudioFormat::new(CHANNELS, SAMPLE_RATE));
    let (bus, _) = Gain::shared(100, MAX_VOLUME);
    let mixer = Mixer::new(
        format.clone(),
        DEFAULT_JITTER_CHUNKS,
        DEFAULT_OUT_FRAMES,
        bus,
    );
    let timing = Arc::new(FillStats::new());
    let stats = Arc::new(OutputStats::new());
    let block_samples = format.channel_count.max(1) as usize * DEFAULT_OUT_FRAMES;
    let (producer, consumer) = RingBuffer::new(block_samples * RING_CAPACITY_BLOCKS);
    let callback_timing = timing.clone();
    let callback_stats = stats.clone();
    let stream = match output {
        Some((device, sample_format, config)) => {
            let mut wrapper =
                send_safe::SendWrapperThread::new(move || {
                    match build_output_stream(
                        sample_format,
                        config,
                        &device,
                        consumer,
                        callback_timing,
                        callback_stats,
                    ) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            log::warn!(
                                "Failed to build output stream, falling back to dummy: {e:?}"
                            );
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
            if play_ok {
                Some(wrapper)
            } else {
                log::warn!("Failed to start output stream, falling back to dummy");
                None
            }
        }
        None => None,
    };
    let (op_tx, op_rx) = mpsc::channel(MIXER_OPS_BOUND);
    let task_stats = stats.clone();
    tokio::spawn(async move {
        run_mixer_owner(mixer, producer, task_stats, op_rx, block_samples).await
    });
    AudioOutput {
        handle: OutputHandle {
            client: MixerClient {
                tx: op_tx,
                dropped: Arc::new(AtomicUsize::new(0)),
            },
            timing,
            format,
            stats,
        },
        _guard: OutputGuard { _stream: stream },
    }
}

fn build_output_stream(
    sample_format: SampleFormat,
    config: StreamConfig,
    device: &Device,
    consumer: Consumer<f32>,
    timing: Arc<FillStats>,
    stats: Arc<OutputStats>,
) -> anyhow::Result<Stream> {
    match sample_format {
        SampleFormat::I8 => run_output::<i8>(config, device, consumer, timing, stats),
        SampleFormat::I16 => run_output::<i16>(config, device, consumer, timing, stats),
        SampleFormat::I32 => run_output::<i32>(config, device, consumer, timing, stats),
        SampleFormat::I64 => run_output::<i64>(config, device, consumer, timing, stats),
        SampleFormat::U8 => run_output::<u8>(config, device, consumer, timing, stats),
        SampleFormat::U16 => run_output::<u16>(config, device, consumer, timing, stats),
        SampleFormat::U32 => run_output::<u32>(config, device, consumer, timing, stats),
        SampleFormat::U64 => run_output::<u64>(config, device, consumer, timing, stats),
        SampleFormat::F32 => run_output::<f32>(config, device, consumer, timing, stats),
        SampleFormat::F64 => run_output::<f64>(config, device, consumer, timing, stats),
        other => Err(anyhow::anyhow!(
            "unsupported output sample format {other:?}"
        )),
    }
}

fn run_output<T>(
    config: StreamConfig,
    device: &Device,
    mut consumer: Consumer<f32>,
    timing: Arc<FillStats>,
    stats: Arc<OutputStats>,
) -> anyhow::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let err_fn = |err| eprintln!("output stream error: {err}");
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let start = std::time::Instant::now();
                for out in data.iter_mut() {
                    let sample = consumer.pop().unwrap_or_else(|_| {
                        stats.note_underrun();
                        0.0
                    });
                    *out = T::from_sample(sample);
                }
                timing.record(start.elapsed());
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow::anyhow!("build output stream: {e}"))
}
