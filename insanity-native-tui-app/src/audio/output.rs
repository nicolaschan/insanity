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
use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry};
use insanity_core::audio::mixer::Mixer;
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::Gain;
use rtrb::{Consumer, Producer, RingBuffer};
use rubato_audio_source::StreamResampler;
use tokio::sync::{mpsc, watch};

use crate::audio::cpal_registry::{CpalAudioDevice, CpalOutputDeviceRegistry};

use super::config::get_output_config;
use super::cpal_stream::sample_format_dispatch;
use super::handoff::{HANDOFF_BOUND, HandoffRequest, Selection};
use super::mixer::{
    AppMixer, MAX_VOLUME, MIXER_OPS_BOUND, MixerClient, MixerOp, SubscribeRequest,
    TARGET_RING_BLOCKS, demand_sleep,
};
use super::stream_errors::{FatalReporter, FatalSignal};

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
const PREFILL_BOUND: usize = 8;

pub struct OutputStats {
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

    pub fn underruns(&self) -> usize {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn overruns(&self) -> usize {
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
    pub(crate) format: AudioFormat,
    pub(crate) timing: Arc<FillStats>,
    pub(crate) stats: Arc<OutputStats>,
}

pub(crate) struct AudioOutput {
    pub(crate) handle: OutputHandle,
    pub(crate) manager: OutputManager,
}

pub struct Sink {
    producer: Producer<f32>,
    stream: Option<send_safe::SendWrapperThread<Option<Stream>>>,
    converter: FormatConverter<StreamResampler>,
    device_format: AudioFormat,
    device_block: usize,
    name: String,
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

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub name: String,
    pub format: AudioFormat,
    pub selection: Selection,
}

#[derive(Clone)]
pub struct OutputManager {
    config: AudioPipelineConfig,
    switch_tx: mpsc::Sender<HandoffRequest<Sink, OutputInfo>>,
    fatal: Arc<FatalSignal>,
    stats: Arc<OutputStats>,
    timing: Arc<FillStats>,
    info: watch::Receiver<OutputInfo>,
    generation: Arc<AtomicU64>,
    current: Selection,
}

impl OutputManager {
    pub fn selection(&self) -> &Selection {
        &self.current
    }

    pub fn subscribe(&self) -> watch::Receiver<OutputInfo> {
        self.info.clone()
    }

    pub fn current_name(&self) -> String {
        self.info.borrow().name.clone()
    }

    pub fn stats(&self) -> Arc<OutputStats> {
        Arc::clone(&self.stats)
    }

    pub fn fatal_signal(&self) -> Arc<FatalSignal> {
        Arc::clone(&self.fatal)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn switch_to(&mut self, id: &str, name: &str) {
        let Some(device) = CpalOutputDeviceRegistry::find(id, name) else {
            log::warn!("Requested output device not found: {name}");
            return;
        };
        self.adopt(
            Selection::Explicit {
                id: id.to_owned(),
                name: name.to_owned(),
            },
            device,
        );
    }

    pub fn follow_default(&mut self) {
        let Some(device) = CpalOutputDeviceRegistry::default_device() else {
            log::warn!("No output device available, falling back to dummy");
            self.adopt_dummy();
            return;
        };
        if !self.adopt(Selection::FollowDefault, device) {
            self.adopt_dummy();
        }
    }

    fn adopt(&mut self, selection: Selection, device: CpalAudioDevice) -> bool {
        let next = self.generation.load(Ordering::Relaxed) + 1;
        let logical = AudioFormat::new(self.config.channels(), self.config.sample_rate());
        let reporter = FatalReporter::new(Arc::clone(&self.fatal), next);
        if let Some((sink, info)) = build_sink(
            device,
            &logical,
            self.config,
            &self.stats,
            &self.timing,
            reporter,
            &selection,
        ) && self.send(sink, info, next)
        {
            self.generation.store(next, Ordering::Relaxed);
            self.current = selection;
            true
        } else {
            false
        }
    }

    fn adopt_dummy(&mut self) {
        let (sink, info) = build_dummy(self.config);
        let next = self.generation.load(Ordering::Relaxed) + 1;
        if self.send(sink, info, next) {
            self.generation.store(next, Ordering::Relaxed);
            self.current = Selection::FollowDefault;
        }
    }

    fn send(&self, sink: Sink, info: OutputInfo, generation: u64) -> bool {
        let name = info.name.clone();
        let request = HandoffRequest {
            payload: sink,
            info,
            generation,
        };
        if self.switch_tx.try_send(request).is_err() {
            log::warn!("Output switch channel full, dropping switch to {name}");
            false
        } else {
            true
        }
    }
}

fn build_sink(
    device: CpalAudioDevice,
    logical: &AudioFormat,
    config: AudioPipelineConfig,
    stats: &Arc<OutputStats>,
    timing: &Arc<FillStats>,
    reporter: FatalReporter,
    selection: &Selection,
) -> Option<(Sink, OutputInfo)> {
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
        logical.clone(),
        device_format.clone(),
        device_block,
        device_block * RING_CAPACITY_BLOCKS,
    );
    let build_timing = Arc::clone(timing);
    let build_stats = Arc::clone(stats);
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        match build_output_stream(
            sample_format,
            cfg,
            &device.0,
            consumer,
            build_timing,
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
    let info = OutputInfo {
        name: name.clone(),
        format: device_format.clone(),
        selection: selection.clone(),
    };
    let sink = Sink {
        producer,
        stream: Some(wrapper),
        converter,
        device_format,
        device_block,
        name,
    };
    Some((sink, info))
}

fn build_dummy(config: AudioPipelineConfig) -> (Sink, OutputInfo) {
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
    let info = OutputInfo {
        name: UNKNOWN_DEVICE_NAME.into(),
        format: format.clone(),
        selection: Selection::FollowDefault,
    };
    let sink = Sink {
        producer,
        stream: None,
        converter,
        device_format: format,
        device_block,
        name: UNKNOWN_DEVICE_NAME.into(),
    };
    (sink, info)
}

pub(crate) fn start_output(audio_config: AudioPipelineConfig) -> AudioOutput {
    let stats = Arc::new(OutputStats::new());
    let fatal = Arc::new(FatalSignal::new());
    let (switch_tx, swap_rx) = mpsc::channel(HANDOFF_BOUND);
    let selection = Selection::FollowDefault;
    let logical = AudioFormat::new(audio_config.channels(), audio_config.sample_rate());
    debug_assert!(logical.channel_count > 0);
    let logical_block = logical.channel_count as usize * audio_config.frames();
    debug_assert!(logical_block > 0);
    let (bus, _) = Gain::shared(100, MAX_VOLUME);
    let mixer = Mixer::new(audio_config, bus);
    let timing = Arc::new(FillStats::new());
    let (initial_sink, initial_info, generation) = match CpalOutputDeviceRegistry::default_device()
    {
        Some(device) => {
            let reporter = FatalReporter::new(Arc::clone(&fatal), 1);
            match build_sink(
                device,
                &logical,
                audio_config,
                &stats,
                &timing,
                reporter,
                &selection,
            ) {
                Some(built) => (built.0, built.1, 1),
                None => {
                    let (sink, info) = build_dummy(audio_config);
                    (sink, info, 0)
                }
            }
        }
        None => {
            log::warn!("No output device available, falling back to dummy");
            let (sink, info) = build_dummy(audio_config);
            (sink, info, 0)
        }
    };
    let initial_format = initial_info.format.clone();
    let (info_tx, info_rx) = watch::channel(initial_info);
    let (op_tx, op_rx) = mpsc::channel(MIXER_OPS_BOUND);
    let task_stats = Arc::clone(&stats);
    tokio::spawn(run_output_owner(
        mixer,
        initial_sink,
        task_stats,
        op_rx,
        swap_rx,
        info_tx,
        logical_block,
    ));
    AudioOutput {
        handle: OutputHandle {
            client: MixerClient {
                tx: op_tx,
                dropped: Arc::new(AtomicUsize::new(0)),
            },
            timing: Arc::clone(&timing),
            format: initial_format,
            stats: Arc::clone(&stats),
        },
        manager: OutputManager {
            config: audio_config,
            switch_tx,
            fatal,
            stats,
            timing,
            info: info_rx,
            generation: Arc::new(AtomicU64::new(generation)),
            current: selection,
        },
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
    request: HandoffRequest<Sink, OutputInfo>,
    mixer: &mut AppMixer,
    logical_block: usize,
    stats: &Arc<OutputStats>,
    info_tx: &watch::Sender<OutputInfo>,
) {
    let mut fresh = request.payload;
    activate_sink(&mut fresh, mixer, logical_block, stats);
    let evicted = std::mem::replace(sink, fresh);
    info_tx.send_replace(request.info);
    log::info!(
        "Output device switched to {} (generation {})",
        sink.name,
        request.generation
    );
    tokio::task::spawn_blocking(move || drop(evicted));
}

pub(crate) async fn run_output_owner(
    mut mixer: AppMixer,
    mut sink: Sink,
    stats: Arc<OutputStats>,
    mut op_rx: mpsc::Receiver<MixerOp>,
    mut swap_rx: mpsc::Receiver<HandoffRequest<Sink, OutputInfo>>,
    info_tx: watch::Sender<OutputInfo>,
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
    timing: Arc<FillStats>,
    stats: Arc<OutputStats>,
    reporter: FatalReporter,
) -> anyhow::Result<Stream> {
    sample_format_dispatch!(
        sample_format,
        run_output,
        config,
        device,
        consumer,
        timing,
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
    timing: Arc<FillStats>,
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
    use super::super::handoff::{HandoffRequest, Selection};
    use super::{OutputManager, build_dummy};
    use super::{activate_sink, adopt_sink};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::config::AudioPipelineConfig;
    use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
    use insanity_core::audio::mixer::Mixer;
    use insanity_core::audio::transform::Gain;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use tokio::sync::{mpsc, watch};

    use super::super::mixer::{AppMixer, MAX_VOLUME};
    use super::super::stream_errors::FatalSignal;

    fn pipeline_config() -> AudioPipelineConfig {
        AudioPipelineConfig::default()
    }

    fn empty_mixer() -> AppMixer {
        let config = pipeline_config();
        let (bus, _) = Gain::shared(100, MAX_VOLUME);
        Mixer::new(config, bus)
    }

    fn logical_block() -> usize {
        let config = pipeline_config();
        config.channels() as usize * config.frames()
    }

    #[test]
    fn build_dummy_matches_pipeline_format() {
        let config = pipeline_config();
        let (sink, info) = build_dummy(config);
        assert_eq!(sink.device_format, config.audio_format());
        assert_eq!(sink.device_block, config.block_samples());
        assert_eq!(info.format, config.audio_format());
        assert_eq!(info.name, UNKNOWN_DEVICE_NAME);
        assert!(matches!(info.selection, Selection::FollowDefault));
    }

    #[tokio::test]
    async fn prefill_converges_within_bound() {
        let config = pipeline_config();
        let (mut sink, _) = build_dummy(config);
        let mut mixer = empty_mixer();
        let stats = Arc::new(super::OutputStats::new());
        assert_eq!(sink.buffered_samples(), 0);
        activate_sink(&mut sink, &mut mixer, logical_block(), &stats);
        assert!(sink.buffered_samples() >= sink.device_block);
        assert!(!sink.play());
    }

    #[tokio::test]
    async fn adopt_replaces_sink_and_publishes_info() {
        let config = pipeline_config();
        let (mut sink, _) = build_dummy(config);
        let (fresh, fresh_info) = build_dummy(config);
        let name = fresh.name.clone();
        let (info_tx, info_rx) = watch::channel(test_info());
        let mut mixer = empty_mixer();
        let stats = Arc::new(super::OutputStats::new());
        adopt_sink(
            &mut sink,
            HandoffRequest {
                payload: fresh,
                info: fresh_info,
                generation: 2,
            },
            &mut mixer,
            logical_block(),
            &stats,
            &info_tx,
        );
        assert_eq!(sink.name, name);
        assert_eq!(info_rx.borrow().name, name);
    }

    fn test_info() -> super::OutputInfo {
        super::OutputInfo {
            name: "initial".into(),
            format: AudioFormat::new(2, 48000),
            selection: Selection::FollowDefault,
        }
    }

    fn test_manager() -> OutputManager {
        let (switch_tx, _) = mpsc::channel(8);
        let (_, info_rx) = watch::channel(test_info());
        OutputManager {
            config: pipeline_config(),
            switch_tx,
            fatal: Arc::new(FatalSignal::new()),
            stats: Default::default(),
            timing: Default::default(),
            info: info_rx,
            generation: Arc::new(AtomicU64::new(0)),
            current: Selection::FollowDefault,
        }
    }

    #[test]
    fn unknown_device_switch_warns_and_keeps_selection() {
        let mut manager = test_manager();
        manager.switch_to("no-such-id", "no-such-device");
        assert!(matches!(manager.selection(), Selection::FollowDefault));
    }

    #[test]
    fn follow_default_never_panics_without_devices() {
        let mut manager = test_manager();
        manager.follow_default();
        assert!(matches!(manager.selection(), Selection::FollowDefault));
    }
}
