use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::converter::FormatConverter;
use insanity_core::audio::device::{AudioDevice, UNKNOWN_DEVICE_NAME};
use insanity_core::audio::mixer::Mixer;
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::Gain;
use rubato_audio_source::StreamResampler;
use tokio::sync::{mpsc, watch};

use super::config::get_output_config;
use super::cpal_registry::{CpalAudioDevice, default_output_device, find_output_by_id_name};
use super::input::Selection;
use super::mixer::{
    AppMixer, MAX_VOLUME, MIXER_OPS_BOUND, MixerClient, MixerOp, TARGET_RING_BLOCKS, demand_sleep,
};
use crate::switching_chunk_source::SwapRequest;

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
    pub(crate) stats: Arc<OutputStats>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    pub name: String,
    pub format: AudioFormat,
    pub selection: Selection,
}

#[derive(Clone)]
pub struct OutputManager {
    switch_tx: mpsc::UnboundedSender<SwapRequest<Sink>>,
    info: watch::Sender<OutputInfo>,
    config: AudioPipelineConfig,
    stats: Arc<OutputStats>,
    timing: Arc<FillStats>,
}

const MAX_PREFILL_TICKS: usize = 1024;

impl OutputManager {
    pub fn current(&self) -> OutputInfo {
        self.info.borrow().clone()
    }

    pub fn switch_to(&self, id: &str, name: &str) -> anyhow::Result<String> {
        let Some(device) = find_output_by_id_name(id, name) else {
            return Err(anyhow::anyhow!(
                "Unknown output device id: {id}, name: {name}"
            ));
        };
        let logical = self.config.audio_format();
        let sink = build_sink(device, &logical, self.config, &self.stats, &self.timing)?;
        self.switch_to_sink(sink, Selection::Explicit(id.to_owned()))
    }

    pub fn follow_default(&self) -> anyhow::Result<String> {
        let sink = resolve_sink_or_silent(
            default_output_device(),
            self.config,
            &self.stats,
            &self.timing,
        );
        self.switch_to_sink(sink, Selection::FollowDefault)
    }

    pub(crate) fn switch_to_sink(
        &self,
        sink: Sink,
        selection: Selection,
    ) -> anyhow::Result<String> {
        let name = sink.name.clone();
        let format = sink.device_format.clone();
        let adopted_name = name.clone();
        let info = self.info.clone();
        self.switch_tx
            .send(SwapRequest {
                payload: sink,
                on_adopt: Box::new(move || {
                    info.send_replace(OutputInfo {
                        name: adopted_name,
                        format,
                        selection,
                    });
                }),
            })
            .map_err(|_| anyhow::anyhow!("Output loop is gone"))?;
        Ok(name)
    }
}

fn dummy_sink(audio_config: AudioPipelineConfig) -> Sink {
    let device_format = audio_config.audio_format();
    let device_block = usize::from(device_format.channel_count) * audio_config.frames();
    let capacity_samples = device_block * RING_CAPACITY_BLOCKS;
    let (producer, _) = rtrb::RingBuffer::new(capacity_samples);
    Sink::new(
        producer,
        None,
        FormatConverter::new(
            device_format.clone(),
            device_format.clone(),
            device_block,
            capacity_samples,
        ),
        device_format,
        device_block,
        UNKNOWN_DEVICE_NAME.into(),
    )
}

fn build_sink(
    device: CpalAudioDevice,
    logical_format: &AudioFormat,
    audio_config: AudioPipelineConfig,
    stats: &Arc<OutputStats>,
    timing: &Arc<FillStats>,
) -> anyhow::Result<Sink> {
    let (sample_format, config) = get_output_config(&device.0, audio_config)
        .map_err(|e| anyhow::anyhow!("Failed to get output config: {e}"))?;
    let name = device.name();
    let device_format = AudioFormat::new(config.channels, config.sample_rate);
    if device_format.channel_count == 0 {
        return Err(anyhow::anyhow!("output channel_count must be > 0"));
    }
    if device_format.sample_rate == 0 {
        return Err(anyhow::anyhow!("device sample_rate must be > 0"));
    }
    let device_block = usize::from(device_format.channel_count) * audio_config.frames();
    if device_block == 0 {
        return Err(anyhow::anyhow!("device block must be > 0"));
    }
    let capacity_samples = device_block * RING_CAPACITY_BLOCKS;
    let (producer, consumer) = rtrb::RingBuffer::new(capacity_samples);
    let callback_timing = timing.clone();
    let callback_stats = stats.clone();
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        match build_output_stream(
            sample_format,
            config,
            &device.0,
            consumer,
            callback_timing,
            callback_stats,
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("Failed to build output stream: {e:?}");
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
        return Err(anyhow::anyhow!("Failed to start output stream"));
    }
    Ok(Sink::new(
        producer,
        Some(wrapper),
        FormatConverter::new(
            logical_format.clone(),
            device_format.clone(),
            device_block,
            capacity_samples,
        ),
        device_format,
        device_block,
        name,
    ))
}

pub(crate) struct Sink {
    producer: rtrb::Producer<f32>,
    _stream: Option<send_safe::SendWrapperThread<Option<cpal::Stream>>>,
    converter: FormatConverter<StreamResampler>,
    device_format: AudioFormat,
    device_block: usize,
    name: String,
}

impl Sink {
    fn new(
        producer: rtrb::Producer<f32>,
        stream: Option<send_safe::SendWrapperThread<Option<cpal::Stream>>>,
        converter: FormatConverter<StreamResampler>,
        device_format: AudioFormat,
        device_block: usize,
        name: String,
    ) -> Self {
        Sink {
            producer,
            _stream: stream,
            converter,
            device_format,
            device_block,
            name,
        }
    }

    fn capacity_samples(&self) -> usize {
        self.device_block * RING_CAPACITY_BLOCKS
    }

    fn target_samples(&self) -> usize {
        self.device_block * TARGET_RING_BLOCKS
    }

    fn tick(&mut self, mixer: &mut AppMixer, logical_block: usize, stats: &Arc<OutputStats>) {
        let ring_needs = self
            .capacity_samples()
            .saturating_sub(self.producer.slots())
            < self.target_samples()
            && self.producer.slots() >= self.device_block;
        if ring_needs && self.converter.pending_samples() < self.device_block {
            let mut logical = Vec::with_capacity(logical_block);
            logical.extend((0..logical_block).map(|_| mixer.next_sync().unwrap_or(0.0)));
            let dropped = self.converter.feed(logical);
            if dropped > 0 {
                stats.note_overrun(dropped);
            }
        }
        for _ in 0..TARGET_RING_BLOCKS {
            if self
                .capacity_samples()
                .saturating_sub(self.producer.slots())
                >= self.target_samples()
            {
                break;
            }
            if self.producer.slots() < self.device_block {
                break;
            }
            let Some(block) = self.converter.take_block() else {
                break;
            };
            match self.producer.write_chunk_uninit(self.device_block) {
                Ok(chunk) => {
                    chunk.fill_from_iter(block);
                }
                Err(_) => {
                    stats.note_overrun(self.device_block);
                    break;
                }
            }
        }
    }

    fn buffered_samples(&self) -> usize {
        self.capacity_samples()
            .saturating_sub(self.producer.slots())
    }
}

pub(crate) fn start_output(audio_config: AudioPipelineConfig) -> (OutputManager, OutputHandle) {
    let timing = Arc::new(FillStats::new());
    let stats = Arc::new(OutputStats::new());
    let initial_sink =
        resolve_sink_or_silent(default_output_device(), audio_config, &stats, &timing);
    spawn_output(audio_config, initial_sink, stats, timing)
}

fn resolve_sink_or_silent(
    initial: Option<CpalAudioDevice>,
    audio_config: AudioPipelineConfig,
    stats: &Arc<OutputStats>,
    timing: &Arc<FillStats>,
) -> Sink {
    let logical_format = audio_config.audio_format();
    match initial {
        Some(device) => match build_sink(device, &logical_format, audio_config, stats, timing) {
            Ok(sink) => sink,
            Err(e) => {
                log::warn!("Failed to open default output, falling back to dummy: {e:?}");
                dummy_sink(audio_config)
            }
        },
        None => dummy_sink(audio_config),
    }
}

fn spawn_output(
    audio_config: AudioPipelineConfig,
    initial: Sink,
    stats: Arc<OutputStats>,
    timing: Arc<FillStats>,
) -> (OutputManager, OutputHandle) {
    let logical_format = audio_config.audio_format();
    let logical_block = usize::from(logical_format.channel_count) * audio_config.frames();
    assert!(logical_block > 0, "logical block must be > 0");
    let mixer = Mixer::new(
        logical_format,
        audio_config,
        Gain::shared(100, MAX_VOLUME).0,
    );
    let (op_tx, op_rx) = mpsc::channel(MIXER_OPS_BOUND);
    let (switch_tx, switch_rx) = mpsc::unbounded_channel();
    let (info_tx, _) = watch::channel(OutputInfo {
        name: initial.name.clone(),
        format: initial.device_format.clone(),
        selection: Selection::FollowDefault,
    });
    let task_stats = stats.clone();
    tokio::spawn(async move {
        run_output_owner(mixer, initial, logical_block, task_stats, op_rx, switch_rx).await
    });
    let manager = OutputManager {
        switch_tx,
        info: info_tx,
        config: audio_config,
        stats: stats.clone(),
        timing: timing.clone(),
    };
    let handle = OutputHandle {
        client: MixerClient {
            tx: op_tx,
            dropped: Arc::new(AtomicUsize::new(0)),
        },
        timing,
        stats,
    };
    (manager, handle)
}

async fn run_output_owner(
    mut mixer: AppMixer,
    mut sink: Sink,
    logical_block: usize,
    stats: Arc<OutputStats>,
    mut op_rx: mpsc::Receiver<MixerOp>,
    mut switch_rx: mpsc::UnboundedReceiver<SwapRequest<Sink>>,
) {
    let mut batch = Vec::with_capacity(MIXER_OPS_BOUND);
    let mut sleep = std::time::Duration::ZERO;
    let mut switch_open = true;
    loop {
        tokio::select! {
            biased;
            count = op_rx.recv_many(&mut batch, MIXER_OPS_BOUND) => {
                if count == 0 {
                    break;
                }
            }
            request = switch_rx.recv(), if switch_open => {
                match request {
                    Some(request) => {
                        // Replace sink with request payload
                        let SwapRequest { payload, on_adopt } = request;
                        let evicted = sink;
                        sink = payload;
                        let mut ticks = 0;
                        while sink.buffered_samples() < sink.device_block
                            && ticks < MAX_PREFILL_TICKS
                        {
                            sink.tick(&mut mixer, logical_block, &stats);
                            ticks += 1;
                        }
                        if ticks >= MAX_PREFILL_TICKS {
                            log::warn!(
                                "Output prefill did not converge; continuing underrun-tolerant"
                            );
                        }
                        (on_adopt)();
                        // Dropping evicted sink in a separate task because dropping a send safe wrapper is weird
                        drop(tokio::task::spawn_blocking(move || drop(evicted)));
                    }
                    None => {
                        switch_open = false;
                    }
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
        sink.tick(&mut mixer, logical_block, &stats);
        sleep = demand_sleep(
            sink.capacity_samples(),
            sink.producer.slots(),
            sink.device_block,
            usize::from(sink.device_format.channel_count),
            sink.device_format.sample_rate,
        );
    }
}

fn build_output_stream(
    sample_format: SampleFormat,
    config: cpal::StreamConfig,
    device: &cpal::Device,
    consumer: rtrb::Consumer<f32>,
    timing: Arc<FillStats>,
    stats: Arc<OutputStats>,
) -> anyhow::Result<cpal::Stream> {
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
    config: cpal::StreamConfig,
    device: &cpal::Device,
    mut consumer: rtrb::Consumer<f32>,
    timing: Arc<FillStats>,
    stats: Arc<OutputStats>,
) -> anyhow::Result<cpal::Stream>
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
    use super::super::input::Selection;
    use super::{FillStats, FormatConverter, OutputStats, RING_CAPACITY_BLOCKS, Sink};
    use super::{dummy_sink, spawn_output};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::config::AudioPipelineConfig;
    use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
    use rtrb::{Consumer, RingBuffer};
    use std::sync::Arc;

    fn synthetic_sink(
        config: AudioPipelineConfig,
        device_format: AudioFormat,
    ) -> (Sink, Consumer<f32>) {
        let device_block = usize::from(device_format.channel_count) * config.frames();
        let capacity_samples = device_block * RING_CAPACITY_BLOCKS;
        let (producer, consumer) = RingBuffer::new(capacity_samples);
        let converter = FormatConverter::new(
            config.audio_format(),
            device_format.clone(),
            device_block,
            capacity_samples,
        );
        (
            Sink::new(
                producer,
                None,
                converter,
                device_format,
                device_block,
                "synthetic".to_owned(),
            ),
            consumer,
        )
    }

    #[tokio::test]
    async fn switches_and_buffers_device_block() {
        let config = AudioPipelineConfig::default();
        let timing = Arc::new(FillStats::new());
        let stats = Arc::new(OutputStats::new());
        let (manager, _handle) =
            spawn_output(config, dummy_sink(config), stats.clone(), timing.clone());
        let device_format = AudioFormat::new(2, 44100);
        let device_block = 2 * config.frames();
        let (sink, consumer) = synthetic_sink(config, device_format.clone());
        manager
            .switch_to_sink(sink, Selection::Explicit("synthetic".to_owned()))
            .expect("switch send");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while manager.current().name != "synthetic" {
            assert!(
                std::time::Instant::now() < deadline,
                "owner loop did not adopt switch"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(manager.current().format, device_format);
        assert!(
            consumer.slots() >= device_block,
            "prefill must buffer a device block, got {}",
            consumer.slots()
        );
    }

    #[tokio::test]
    async fn follow_default_converges_to_follow_default_selection() {
        let config = AudioPipelineConfig::default();
        let timing = Arc::new(FillStats::new());
        let stats = Arc::new(OutputStats::new());
        let (manager, _handle) =
            spawn_output(config, dummy_sink(config), stats.clone(), timing.clone());
        let name = manager.follow_default().expect("follow_default sends");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while manager.current().name != name {
            assert!(
                std::time::Instant::now() < deadline,
                "owner loop did not adopt follow_default"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(manager.current().selection, Selection::FollowDefault);
    }

    #[tokio::test]
    async fn unknown_id_switch_errors_and_sink_undisturbed() {
        let config = AudioPipelineConfig::default();
        let timing = Arc::new(FillStats::new());
        let stats = Arc::new(OutputStats::new());
        let (manager, _handle) =
            spawn_output(config, dummy_sink(config), stats.clone(), timing.clone());
        assert!(
            manager
                .switch_to("no-such-device", "No Such Device")
                .is_err()
        );
        assert_eq!(manager.current().selection, Selection::FollowDefault);
        assert_eq!(manager.current().name, UNKNOWN_DEVICE_NAME);
    }

    #[tokio::test]
    async fn owner_stays_responsive_after_managers_drop() {
        let config = AudioPipelineConfig::default();
        let timing = Arc::new(FillStats::new());
        let stats = Arc::new(OutputStats::new());
        let (manager, handle) =
            spawn_output(config, dummy_sink(config), stats.clone(), timing.clone());
        drop(manager);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let snapshot =
            tokio::time::timeout(std::time::Duration::from_secs(2), handle.client.snapshot())
                .await
                .expect("owner loop must stay responsive after managers drop");
        assert!(snapshot.is_some(), "owner loop must survive manager drop");
    }
}
