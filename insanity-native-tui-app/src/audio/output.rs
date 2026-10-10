use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::converter::FormatConverter;
use insanity_core::audio::device::AudioDevice;
use insanity_core::audio::mixer::Mixer;
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::Gain;
use rtrb::{Consumer, Producer, RingBuffer};
use rubato_audio_source::StreamResampler;
use tokio::sync::{mpsc, watch};

use crate::audio::{
    cpal_registry::{CpalAudioDevice, CpalOutputDeviceRegistry},
    device_supervisor::{DUMMY_DEVICE_NAME, DeviceInfo, DeviceManager, PayloadBuilder},
};

use super::config::{STREAM_BUILD_TIMEOUT, get_output_config};
use super::cpal_stream::sample_format_dispatch;
use super::handoff::{HANDOFF_BOUND, HandoffRequest};
use super::mixer::{
    AppMixer, MAX_VOLUME, MIXER_OPS_BOUND, MixerClient, MixerOp, RING_CAPACITY_BLOCKS,
    SubscribeRequest, TARGET_RING_BLOCKS, demand_sleep,
};
use super::stream_errors::{FatalReporter, FatalSignal};

const PREFILL_BOUND: usize = 8;

#[derive(Default)]
pub(crate) struct OutputStats {
    underruns: AtomicUsize,
    overruns: AtomicUsize,
    total_nanos: AtomicU64,
    fills: AtomicUsize,
    data_callbacks: AtomicU64,
}

impl OutputStats {
    fn note_underruns(&self, samples: usize) {
        self.underruns.fetch_add(samples, Ordering::Relaxed);
    }

    pub(crate) fn note_overrun(&self, samples: usize) {
        self.overruns.fetch_add(samples, Ordering::Relaxed);
    }

    fn note_data_callback(&self) {
        self.data_callbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn data_callbacks(&self) -> u64 {
        self.data_callbacks.load(Ordering::Relaxed)
    }

    pub(crate) fn underruns(&self) -> usize {
        self.underruns.load(Ordering::Relaxed)
    }

    pub(crate) fn overruns(&self) -> usize {
        self.overruns.load(Ordering::Relaxed)
    }

    pub(crate) fn record(&self, elapsed: std::time::Duration) {
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
    pub(crate) manager: OutputManager,
}

pub(crate) struct Sink {
    producer: Producer<f32>,
    stream: Option<send_safe::SendWrapperThread<Option<Stream>>>,
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
                .execute(|stream| stream.as_ref().is_some_and(|active| active.play().is_ok()))
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
            match self.producer.write_chunk_uninit(block.len()) {
                Ok(chunk) => {
                    chunk.fill_from_iter(block);
                }
                Err(_) => {
                    debug_assert!(false, "ring slots were checked");
                    stats.note_overrun(block.len());
                    break;
                }
            }
        }
    }
}

pub(crate) struct SinkBuilder;

impl PayloadBuilder for SinkBuilder {
    type Payload = Sink;
    type Stats = OutputStats;

    fn build(
        device: CpalAudioDevice,
        config: &AudioPipelineConfig,
        stats: &Arc<Self::Stats>,
        reporter: FatalReporter,
    ) -> Option<(Self::Payload, DeviceInfo)> {
        let name = device.name();
        let Ok((sample_format, cfg)) = get_output_config(&device.0, config) else {
            log::warn!("Failed to get output config for {name}, falling back to dummy");
            return None;
        };
        let device_format = AudioFormat::new(cfg.channels, cfg.sample_rate);
        let device_block = cfg.channels as usize * config.frames();
        assert!(device_block > 0);
        let (producer, consumer) = RingBuffer::new(device_block * RING_CAPACITY_BLOCKS);
        let converter = FormatConverter::<StreamResampler>::new(
            config.audio_format().clone(),
            device_format.clone(),
            device_block,
            device_block * RING_CAPACITY_BLOCKS,
        );
        let build_stats = Arc::clone(stats);
        let mut wrapper = send_safe::SendWrapperThread::new(move || {
            match build_output_stream(
                sample_format,
                cfg,
                &device.0,
                consumer,
                build_stats,
                reporter,
            ) {
                Ok(stream) => Some(stream),
                Err(e) => {
                    log::warn!("Failed to build output stream, falling back to dummy: {e:?}");
                    None
                }
            }
        });
        let playing = wrapper
            .execute(|stream| stream.as_ref().is_some_and(|active| active.play().is_ok()))
            .unwrap_or(false);
        if !playing {
            log::warn!("Failed to start output stream, falling back to dummy");
            return None;
        }
        let info = DeviceInfo {
            name,
            format: device_format.clone(),
        };
        let sink = Sink {
            producer,
            stream: Some(wrapper),
            converter,
            device_format,
            device_block,
        };
        Some((sink, info))
    }

    fn build_dummy(config: &AudioPipelineConfig) -> (Self::Payload, DeviceInfo) {
        let format = AudioFormat::new(config.channels(), config.sample_rate());
        let device_block = format.channel_count as usize * config.frames();
        assert!(device_block > 0);
        let (producer, _) = RingBuffer::new(device_block * RING_CAPACITY_BLOCKS);
        let converter = FormatConverter::<StreamResampler>::new(
            format.clone(),
            format.clone(),
            device_block,
            device_block * RING_CAPACITY_BLOCKS,
        );
        let info = DeviceInfo {
            name: DUMMY_DEVICE_NAME.into(),
            format: format.clone(),
        };
        let sink = Sink {
            producer,
            stream: None,
            converter,
            device_format: format,
            device_block,
        };
        (sink, info)
    }
}

pub(crate) type OutputManager =
    DeviceManager<Sink, OutputStats, CpalOutputDeviceRegistry, SinkBuilder>;

pub(crate) fn start_output(audio_config: AudioPipelineConfig) -> AudioOutput {
    let (switch_tx, swap_rx) = mpsc::channel(HANDOFF_BOUND);
    let (dummy_sink, dummy_info) = SinkBuilder::build_dummy(&audio_config);
    let (info_tx, info_rx) = watch::channel(dummy_info);
    let (op_tx, op_rx) = mpsc::channel(MIXER_OPS_BOUND);

    let mut manager = OutputManager::new(
        audio_config,
        switch_tx,
        Arc::new(FatalSignal::new()),
        Arc::new(OutputStats::default()),
        info_rx,
        Arc::new(AtomicU64::new(0)),
    );

    tokio::spawn(run_output_owner(
        Mixer::new(audio_config, Gain::shared(100, MAX_VOLUME).0),
        dummy_sink,
        manager.stats(),
        op_rx,
        swap_rx,
        info_tx,
        audio_config.block_samples(),
    ));

    // Request starting the actual output device
    manager.select_current_default();
    AudioOutput {
        client: MixerClient {
            tx: op_tx,
            dropped: Arc::new(AtomicUsize::new(0)),
        },
        manager,
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

fn adopt_sink(
    sink: &mut Sink,
    request: HandoffRequest<Sink, DeviceInfo>,
    mixer: &mut AppMixer,
    logical_block: usize,
    stats: &Arc<OutputStats>,
    info_tx: &watch::Sender<DeviceInfo>,
) {
    let mut fresh = request.payload;
    activate_sink(&mut fresh, mixer, logical_block, stats);
    let evicted = std::mem::replace(sink, fresh);
    log::info!(
        "Output device switched to {} (generation {} channels={} rate={})",
        request.info.name,
        request.generation,
        request.info.format.channel_count,
        request.info.format.sample_rate,
    );
    info_tx.send_replace(request.info);
    tokio::task::spawn_blocking(move || drop(evicted));
}

async fn run_output_owner(
    mut mixer: AppMixer,
    mut sink: Sink,
    stats: Arc<OutputStats>,
    mut op_rx: mpsc::Receiver<MixerOp>,
    mut swap_rx: mpsc::Receiver<HandoffRequest<Sink, DeviceInfo>>,
    info_tx: watch::Sender<DeviceInfo>,
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
            swapped = swap_rx.recv() => {
                let Some(request) = swapped else { break };
                adopt_sink(&mut sink, request, &mut mixer, logical_block, &stats, &info_tx);
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
    reporter: FatalReporter,
) -> anyhow::Result<Stream> {
    sample_format_dispatch!(
        sample_format,
        run_output,
        config,
        device,
        consumer,
        stats,
        reporter
    )
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
    reporter: FatalReporter,
) -> anyhow::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let err_fn = move |err: cpal::Error| {
        reporter.report_output(err.kind());
    };
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                stats.note_data_callback();
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
