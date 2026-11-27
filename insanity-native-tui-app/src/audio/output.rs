use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

use cpal::{
    Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, StreamTrait},
};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::converter::FormatConverter;
use insanity_core::audio::mixer::Mixer;
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::{Gain, GainControl};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato_audio_source::StreamResampler;
use tokio::sync::mpsc;

use super::config::{STREAM_BUILD_TIMEOUT, get_output_config};
use super::cpal_registry::{default_output_device, device_name};
use super::cpal_stream::sample_format_dispatch;
use super::mixer::{
    AppMixer, MAX_VOLUME, MIXER_OPS_BOUND, MixerClient, MixerOp, RING_CAPACITY_BLOCKS,
    SubscribeRequest, TARGET_RING_BLOCKS, demand_sleep,
};

const PREFILL_BOUND: usize = 8;

#[derive(Default)]
pub(crate) struct OutputStats {
    underruns: AtomicUsize,
    overruns: AtomicUsize,
    total_nanos: AtomicU64,
    fills: AtomicUsize,
}

impl OutputStats {
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

    pub(crate) fn record(&self, elapsed: Duration) {
        self.fills.fetch_add(1, Ordering::Relaxed);
        self.total_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub(crate) fn avg_nanos(&self) -> u64 {
        let fills = self.fills.load(Ordering::Relaxed) as u64;
        if fills == 0 {
            return 0;
        }
        self.total_nanos.load(Ordering::Relaxed) / fills
    }
}

#[derive(Clone)]
pub(crate) struct AudioOutput {
    pub(crate) client: MixerClient,
    pub(crate) stats: Arc<OutputStats>,
    speaker_gain: Arc<GainControl>,
}

impl AudioOutput {
    pub(crate) fn set_speaker_volume(&self, volume: usize) {
        self.speaker_gain.set(volume);
    }

    pub(crate) fn speaker_volume(&self) -> usize {
        self.speaker_gain.get()
    }
}

struct Sink {
    producer: Producer<f32>,
    stream: Option<send_safe::SendWrapperThread<Result<Stream, String>>>,
    converter: FormatConverter<StreamResampler>,
    device_format: AudioFormat,
    device_block: usize,
}

impl Sink {
    fn buffered_samples(&self) -> usize {
        self.device_block * RING_CAPACITY_BLOCKS - self.producer.slots()
            + self.converter.pending_samples()
    }

    fn play(&mut self) -> bool {
        match self.stream.as_mut() {
            Some(wrapper) => wrapper
                .execute(|slot| matches!(slot, Ok(active) if active.play().is_ok()))
                .unwrap_or(false),
            None => false,
        }
    }

    fn tick(&mut self, mixer: &mut AppMixer, logical_block: usize, stats: &Arc<OutputStats>) {
        let logical: Vec<f32> = (0..logical_block)
            .map(|_| mixer.next_sync().unwrap_or(0.0))
            .collect();
        self.converter.feed(logical);
        while self.converter.pending_samples() >= self.device_block {
            if self.producer.slots() < self.device_block {
                break;
            }
            let Some(block) = self.converter.take_block() else {
                debug_assert!(false, "pending samples imply a full block");
                break;
            };
            let Ok(chunk) = self.producer.write_chunk_uninit(block.len()) else {
                debug_assert!(false, "ring slots were checked");
                stats.note_overrun(block.len());
                break;
            };
            chunk.fill_from_iter(block);
        }
    }
}

fn build_device_sink(
    device: Device,
    audio_config: &AudioPipelineConfig,
    stats: &Arc<OutputStats>,
) -> anyhow::Result<Sink> {
    let (sample_format, cfg) = get_output_config(&device, audio_config)?;
    let channels = cfg.channels;
    let sample_rate = cfg.sample_rate;
    let device_format = AudioFormat::new(channels, sample_rate);
    let device_block = channels as usize * audio_config.frames();
    anyhow::ensure!(device_block > 0, "invalid block size");
    let (producer, consumer) = RingBuffer::new(device_block * RING_CAPACITY_BLOCKS);
    let converter = FormatConverter::<StreamResampler>::new(
        audio_config.audio_format(),
        device_format.clone(),
        device_block,
        device_block * RING_CAPACITY_BLOCKS,
    );
    let build_stats = Arc::clone(stats);
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        build_output_stream(sample_format, cfg, &device, consumer, build_stats)
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
        anyhow::bail!("{sample_format:?} {channels}ch {sample_rate}Hz: {reason}");
    }
    let actual: Option<u32> = wrapper
        .execute(|r| r.as_ref().ok()?.buffer_size().ok())
        .ok()
        .flatten();
    log::info!(
        "Selected output stream: {}ch {}Hz {} req={:?} actual={}",
        channels,
        sample_rate,
        sample_format,
        cfg.buffer_size,
        actual
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    Ok(Sink {
        producer,
        stream: Some(wrapper),
        converter,
        device_format,
        device_block,
    })
}

fn build_dummy_sink(audio_config: &AudioPipelineConfig) -> Sink {
    let format = AudioFormat::new(audio_config.channels(), audio_config.sample_rate());
    let device_block = format.channel_count as usize * audio_config.frames();
    assert!(device_block > 0);
    let (producer, _) = RingBuffer::new(device_block * RING_CAPACITY_BLOCKS);
    let converter = FormatConverter::<StreamResampler>::new(
        format.clone(),
        format.clone(),
        device_block,
        device_block * RING_CAPACITY_BLOCKS,
    );
    Sink {
        producer,
        stream: None,
        converter,
        device_format: format,
        device_block,
    }
}

pub(crate) fn start_output(audio_config: AudioPipelineConfig) -> AudioOutput {
    let stats = Arc::new(OutputStats::default());
    let sink = match default_output_device() {
        Some(device) => {
            let name = device_name(&device);
            match build_device_sink(device, &audio_config, &stats) {
                Ok(sink) => {
                    log::info!("Using default output device: {name}");
                    sink
                }
                Err(e) => {
                    log::warn!(
                        "Failed to start output stream on {name}, falling back to dummy: {e:#}"
                    );
                    build_dummy_sink(&audio_config)
                }
            }
        }
        None => {
            log::warn!("No default output device available, falling back to dummy");
            build_dummy_sink(&audio_config)
        }
    };

    let (op_tx, op_rx) = mpsc::channel(MIXER_OPS_BOUND);
    let (bus, speaker_gain) = Gain::shared(100, MAX_VOLUME);
    tokio::spawn(run_output_owner(
        Mixer::new(audio_config, bus),
        sink,
        Arc::clone(&stats),
        op_rx,
        audio_config.block_samples(),
    ));

    AudioOutput {
        client: MixerClient {
            tx: op_tx,
            dropped: Arc::new(AtomicUsize::new(0)),
        },
        stats,
        speaker_gain,
    }
}

fn activate_sink(
    sink: &mut Sink,
    mixer: &mut AppMixer,
    logical_block: usize,
    stats: &Arc<OutputStats>,
) {
    let mut ticks = 0;
    while sink.buffered_samples() < sink.device_block && ticks < PREFILL_BOUND {
        let before = sink.buffered_samples();
        sink.tick(mixer, logical_block, stats);
        ticks += 1;
        if sink.buffered_samples() == before {
            break;
        }
    }
    sink.play();
}

async fn run_output_owner(
    mut mixer: AppMixer,
    mut sink: Sink,
    stats: Arc<OutputStats>,
    mut op_rx: mpsc::Receiver<MixerOp>,
    logical_block: usize,
) {
    debug_assert!(logical_block > 0);
    debug_assert!(sink.device_block > 0);
    activate_sink(&mut sink, &mut mixer, logical_block, &stats);
    let mut batch = Vec::with_capacity(MIXER_OPS_BOUND);
    let mut slot_replies = Vec::new();
    let mut snapshot_replies = Vec::new();
    let mut sleep = Duration::ZERO;
    loop {
        tokio::select! {
            count = op_rx.recv_many(&mut batch, MIXER_OPS_BOUND) => {
                if count == 0 {
                    break;
                }
            }
            _ = tokio::time::sleep(sleep) => {}
        }
        for op in batch.drain(..) {
            match op {
                MixerOp::Push { slot, chunk } => {
                    mixer.push_to_slot(slot, chunk);
                }
                MixerOp::Subscribe(request) => {
                    let SubscribeRequest {
                        transform,
                        decoder,
                        reply,
                    } = *request;
                    let slot = mixer.subscribe(transform, decoder);
                    slot_replies.push((reply, slot));
                }
                MixerOp::Unsubscribe(slot) => mixer.unsubscribe(slot),
                MixerOp::Snapshot(reply) => {
                    snapshot_replies.push((reply, (mixer.metrics_snapshot(), mixer.peer_count())));
                }
            }
        }
        for (reply, slot) in slot_replies.drain(..) {
            let _ = reply.send(slot);
        }
        for (reply, snapshot) in snapshot_replies.drain(..) {
            let _ = reply.send(snapshot);
        }
        let target_samples = sink.device_block * TARGET_RING_BLOCKS;
        for _ in 0..TARGET_RING_BLOCKS {
            if sink.buffered_samples() >= target_samples {
                break;
            }
            sink.tick(&mut mixer, logical_block, &stats);
        }
        sleep = demand_sleep(
            sink.device_block * RING_CAPACITY_BLOCKS,
            sink.producer.slots(),
            sink.device_block,
            usize::from(sink.device_format.channel_count),
            sink.device_format.sample_rate,
        );
    }
}

fn build_output_stream(
    sample_format: SampleFormat,
    config: StreamConfig,
    device: &Device,
    consumer: Consumer<f32>,
    stats: Arc<OutputStats>,
) -> anyhow::Result<Stream> {
    sample_format_dispatch!(sample_format, run_output, config, device, consumer, stats)
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
                stats.record(start.elapsed());
            },
            err_fn,
            Some(STREAM_BUILD_TIMEOUT),
        )
        .map_err(|e| anyhow::anyhow!("build output stream: {e}"))
}
