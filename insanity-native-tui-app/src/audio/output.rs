use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::converter::FormatConverter;
use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
use insanity_core::audio::mixer::Mixer;
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::Gain;
use rtrb::{Consumer, Producer, RingBuffer};
use rubato_audio_source::StreamResampler;
use tokio::sync::mpsc;

use super::config::get_output_config;
use super::cpal_registry::{default_output_device, device_name};
use super::mixer::{
    AppMixer, MAX_VOLUME, MIXER_OPS_BOUND, MixerClient, MixerOp, TARGET_RING_BLOCKS, demand_sleep,
};

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

    fn note_underruns(&self, samples: usize) {
        self.underruns.fetch_add(samples, Ordering::Relaxed);
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
    pub(crate) name: String,
}

pub(crate) struct OutputGuard {
    _stream: Option<send_safe::SendWrapperThread<Option<Stream>>>,
}

pub(crate) struct AudioOutput {
    pub(crate) handle: OutputHandle,
    pub(crate) _guard: OutputGuard,
}

pub(crate) fn start_output(audio_config: AudioPipelineConfig) -> AudioOutput {
    let output = default_output_device().and_then(|device| {
        get_output_config(&device.0, audio_config)
            .inspect_err(|e| log::warn!("Failed to get output config, falling back to dummy: {e}"))
            .ok()
            .map(|(sample_format, config)| (device.0, sample_format, config))
    });
    let name = output.as_ref().map_or_else(
        || UNKNOWN_DEVICE_NAME.into(),
        |(device, _, _)| device_name(device),
    );
    let device_format = output.as_ref().map_or_else(
        || audio_config.audio_format(),
        |(_, _, config)| AudioFormat::new(config.channels, config.sample_rate),
    );
    let logical = audio_config.audio_format();
    let (bus, _) = Gain::shared(100, MAX_VOLUME);
    let mixer = Mixer::new(logical, audio_config, bus);
    let timing = Arc::new(FillStats::new());
    let stats = Arc::new(OutputStats::new());
    assert!(
        device_format.channel_count > 0,
        "output channel_count must be > 0"
    );
    let block_samples = device_format.channel_count as usize * audio_config.frames();
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
    let task_format = device_format.clone();
    tokio::spawn(async move {
        run_output_owner(
            mixer,
            producer,
            task_stats,
            op_rx,
            audio_config,
            task_format,
        )
        .await
    });
    AudioOutput {
        handle: OutputHandle {
            client: MixerClient {
                tx: op_tx,
                dropped: Arc::new(AtomicUsize::new(0)),
            },
            timing,
            format: device_format,
            stats,
            name,
        },
        _guard: OutputGuard { _stream: stream },
    }
}

async fn run_output_owner(
    mut mixer: AppMixer,
    mut ring: Producer<f32>,
    stats: Arc<OutputStats>,
    mut rx: mpsc::Receiver<MixerOp>,
    audio_config: AudioPipelineConfig,
    device_format: AudioFormat,
) {
    let logical_format = audio_config.audio_format();
    let logical_block = usize::from(logical_format.channel_count) * audio_config.frames();
    let device_block = usize::from(device_format.channel_count) * audio_config.frames();
    assert!(logical_block > 0, "logical block must be > 0");
    assert!(device_block > 0, "device block must be > 0");
    assert!(
        device_format.sample_rate > 0,
        "device sample_rate must be > 0"
    );
    let capacity_samples = device_block * RING_CAPACITY_BLOCKS;
    let target_samples = device_block * TARGET_RING_BLOCKS;
    let mut converter: FormatConverter<StreamResampler> = FormatConverter::new(
        logical_format,
        device_format.clone(),
        device_block,
        capacity_samples,
    );
    let mut batch = Vec::with_capacity(MIXER_OPS_BOUND);
    let mut sleep = std::time::Duration::ZERO;
    loop {
        tokio::select! {
            biased;
            count = rx.recv_many(&mut batch, MIXER_OPS_BOUND) => {
                if count == 0 {
                    break;
                }
            }
            _ = tokio::time::sleep(sleep) => {}
        }
        let mut slot_replies = Vec::new();
        let mut snapshot_replies = Vec::new();
        for op in batch.drain(..) {
            match op {
                MixerOp::Push { slot, chunk } => {
                    mixer.push_to_slot(slot, chunk);
                }
                MixerOp::Subscribe(request) => {
                    let slot = mixer.subscribe(request.transform, request.decoder);
                    slot_replies.push((request.reply, slot));
                }
                MixerOp::Unsubscribe(slot) => mixer.unsubscribe(slot),
                MixerOp::Snapshot(reply) => {
                    snapshot_replies.push((reply, (mixer.metrics_snapshot(), mixer.peer_count())));
                }
            }
        }
        for (reply, slot) in slot_replies {
            let _ = reply.send(slot);
        }
        for (reply, snapshot) in snapshot_replies {
            let _ = reply.send(snapshot);
        }
        let ring_needs = capacity_samples.saturating_sub(ring.slots()) < target_samples
            && ring.slots() >= device_block;
        if ring_needs && converter.pending_samples() < device_block {
            let mut logical = Vec::with_capacity(logical_block);
            logical.extend((0..logical_block).map(|_| mixer.next_sync().unwrap_or(0.0)));
            let dropped = converter.feed(logical);
            if dropped > 0 {
                stats.note_overrun(dropped);
            }
        }
        for _ in 0..TARGET_RING_BLOCKS {
            if capacity_samples.saturating_sub(ring.slots()) >= target_samples {
                break;
            }
            if ring.slots() < device_block {
                break;
            }
            let Some(block) = converter.take_block() else {
                break;
            };
            match ring.write_chunk_uninit(device_block) {
                Ok(chunk) => {
                    chunk.fill_from_iter(block);
                }
                Err(_) => {
                    stats.note_overrun(device_block);
                    break;
                }
            }
        }
        sleep = demand_sleep(
            capacity_samples,
            ring.slots(),
            device_block,
            usize::from(device_format.channel_count),
            device_format.sample_rate,
        );
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

fn render_samples<T>(outs: &mut [T], first: &[f32], second: &[f32])
where
    T: SizedSample + FromSample<f32>,
{
    let mut outs = outs.iter_mut();
    for sample in first.iter().chain(second.iter()) {
        if let Some(out) = outs.next() {
            *out = T::from_sample(*sample);
        }
    }
    for out in outs {
        *out = T::from_sample(0.0);
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
    let err_fn = |err: cpal::Error| super::stream_errors::note_output_error(err.kind());
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let start = std::time::Instant::now();
                let available = consumer.slots().min(data.len());
                match consumer.read_chunk(available) {
                    Ok(chunk) => {
                        let (first, second) = chunk.as_slices();
                        render_samples(data, first, second);
                        chunk.commit_all();
                    }
                    Err(_) => render_samples(data, &[], &[]),
                }
                stats.note_underruns(data.len() - available);
                timing.record(start.elapsed());
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow::anyhow!("build output stream: {e}"))
}

#[cfg(test)]
mod tests {
    use super::FormatConverter;
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::config::AudioPipelineConfig;
    use rubato_audio_source::StreamResampler;

    #[test]
    fn resample_48k_to_44100_produces_expected_count() {
        let config = AudioPipelineConfig::default();
        let from = config.audio_format();
        let to = AudioFormat::new(2, 44100);
        let device_block = 2 * config.frames();
        let mut converter: FormatConverter<StreamResampler> =
            FormatConverter::new(from, to, device_block, device_block * 16);
        let mut total = 0;
        for _ in 0..40 {
            assert_eq!(converter.feed(vec![0.5; config.block_samples()]), 0);
            while let Some(block) = converter.take_block() {
                total += block.len();
            }
        }
        total += converter.pending_samples();
        let expected = 40 * config.block_samples() * 44100 / 48000;
        assert!(
            total.abs_diff(expected) <= device_block,
            "total={total} expected={expected}"
        );
    }
}
