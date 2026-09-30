use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use insanity_core::audio::AudioFormat;
use insanity_core::audio::codec::EncodedChunk;
use insanity_core::audio::mixer::{Mixer, MixerMetrics, SlotId};
use insanity_core::audio::transform::{
    ChunkTransform, Denoise, DenoiseControl, Gain, GainControl, Link, MetricsReader, MetricsState,
};
#[cfg(feature = "denoise-passthrough")]
use insanity_core::audio::transform::{Hysteresis, PassthroughGate, RmsDetector};
use insanity_core::user_input_event::DenoiseSelection;
use tokio::sync::{mpsc, oneshot};

use super::codec::OpusDecoder;
use super::denoise::NnnoiselessDenoiser;

pub const MAX_VOLUME: usize = 500;
#[cfg(any(feature = "encode-silence", feature = "denoise-passthrough"))]
pub const QUIET_RMS_THRESHOLD: f32 = 0.0075;
#[cfg(any(feature = "encode-silence", feature = "denoise-passthrough"))]
pub const QUIET_HANGOVER_CHUNKS: usize = 30;

#[cfg(feature = "denoise-passthrough")]
pub type PeerChain = Link<
    PassthroughGate<Denoise<NnnoiselessDenoiser>, Hysteresis<RmsDetector>>,
    Link<Gain, MetricsReader>,
>;
#[cfg(not(feature = "denoise-passthrough"))]
pub type PeerChain = Link<Denoise<NnnoiselessDenoiser>, Link<Gain, MetricsReader>>;
pub(crate) type OpusRebuild = fn(&AudioFormat) -> Option<OpusDecoder>;
pub type AppMixer = Mixer<OpusDecoder, PeerChain, Gain, OpusRebuild>;

#[derive(Clone)]
pub struct PeerControls {
    pub gain: Arc<GainControl>,
    pub denoise: Arc<DenoiseControl>,
    pub loudness: Arc<MetricsState>,
}

impl PeerControls {
    pub fn new(volume: usize, denoise: DenoiseSelection) -> Self {
        let (_, gain) = Gain::shared(volume, MAX_VOLUME);
        let (_, denoise) = Denoise::<NnnoiselessDenoiser>::shared(denoise);
        let (_, loudness) = MetricsReader::shared();
        PeerControls {
            gain,
            denoise,
            loudness,
        }
    }
}

#[cfg(feature = "denoise-passthrough")]
pub fn chain_from_controls(controls: &PeerControls) -> PeerChain {
    PassthroughGate::new(
        Denoise::new(controls.denoise.clone()),
        Hysteresis::new(RmsDetector::new(QUIET_RMS_THRESHOLD), QUIET_HANGOVER_CHUNKS),
    )
    .chain(Gain::new(controls.gain.clone()).chain(MetricsReader::new(controls.loudness.clone())))
}

#[cfg(not(feature = "denoise-passthrough"))]
pub fn chain_from_controls(controls: &PeerControls) -> PeerChain {
    Denoise::new(controls.denoise.clone()).chain(
        Gain::new(controls.gain.clone()).chain(MetricsReader::new(controls.loudness.clone())),
    )
}

pub fn rebuild_opus_decoder(format: &AudioFormat) -> Option<OpusDecoder> {
    OpusDecoder::new(format.sample_rate, format.channel_count)
}

pub const MIXER_OPS_BOUND: usize = 64;
pub const TARGET_RING_BLOCKS: usize = 2;
const MIN_FILL_SLEEP: Duration = Duration::from_millis(2);
const MAX_FILL_SLEEP: Duration = Duration::from_millis(30);

pub(crate) fn demand_sleep(
    capacity_samples: usize,
    free_samples: usize,
    block_samples: usize,
    channels: usize,
    sample_rate: u32,
) -> Duration {
    if block_samples == 0 || channels == 0 || sample_rate == 0 {
        return MAX_FILL_SLEEP;
    }
    let occupied = capacity_samples.saturating_sub(free_samples);
    let target = block_samples.saturating_mul(TARGET_RING_BLOCKS);
    let excess = occupied.saturating_sub(target);
    if excess == 0 {
        return MIN_FILL_SLEEP;
    }
    let samples_per_sec = channels as u64 * u64::from(sample_rate);
    let nanos = excess as u64 * 1_000_000_000 / samples_per_sec;
    Duration::from_nanos(nanos).clamp(MIN_FILL_SLEEP, MAX_FILL_SLEEP)
}

pub(crate) struct SubscribeRequest {
    pub(crate) transform: PeerChain,
    pub(crate) decoder: OpusRebuild,
    pub(crate) reply: oneshot::Sender<SlotId>,
}

pub(crate) enum MixerOp {
    Push { slot: SlotId, chunk: EncodedChunk },
    Subscribe(Box<SubscribeRequest>),
    Unsubscribe(SlotId),
    Snapshot(oneshot::Sender<(MixerMetrics, usize)>),
}

#[derive(Clone)]
pub(crate) struct MixerClient {
    pub(crate) tx: mpsc::Sender<MixerOp>,
    pub(crate) dropped: Arc<AtomicUsize>,
}

impl MixerClient {
    pub(crate) fn push_frame(&self, slot: SlotId, chunk: EncodedChunk) -> bool {
        if self.tx.try_send(MixerOp::Push { slot, chunk }).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    pub(crate) fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    pub(crate) async fn subscribe(
        &self,
        transform: PeerChain,
        decoder: OpusRebuild,
    ) -> Option<SlotId> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MixerOp::Subscribe(Box::new(SubscribeRequest {
                transform,
                decoder,
                reply: reply_tx,
            })))
            .await
            .ok()?;
        reply_rx.await.ok()
    }

    pub(crate) async fn unsubscribe(&self, slot: SlotId) {
        let _ = self.tx.send(MixerOp::Unsubscribe(slot)).await;
    }

    pub(crate) async fn snapshot(&self) -> Option<(MixerMetrics, usize)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send(MixerOp::Snapshot(reply_tx)).await.ok()?;
        reply_rx.await.ok()
    }
}

pub fn format_audio_interval(
    prev: &MixerMetrics,
    current: &MixerMetrics,
    fill_avg_nanos: u64,
    peer_count: usize,
    dropped: usize,
    ring_underruns: usize,
    ring_overruns: usize,
) -> String {
    format!(
        "audio gaps={} late={} overflow={} underruns={} plc={} clips={} fills={} stale={} fill_avg_ns={} peers={} dropped={} ring_underruns={} ring_overruns={}",
        current.gap_detected.saturating_sub(prev.gap_detected),
        current.late_dropped.saturating_sub(prev.late_dropped),
        current
            .overflow_dropped
            .saturating_sub(prev.overflow_dropped),
        current.underrun.saturating_sub(prev.underrun),
        current.plc_hold.saturating_sub(prev.plc_hold),
        current.clip_hits.saturating_sub(prev.clip_hits),
        current.fills.saturating_sub(prev.fills),
        current.stale_dropped.saturating_sub(prev.stale_dropped),
        fill_avg_nanos,
        peer_count,
        dropped,
        ring_underruns,
        ring_overruns,
    )
}

#[cfg(test)]
mod demand_sleep_tests {
    use super::{MAX_FILL_SLEEP, MIN_FILL_SLEEP, TARGET_RING_BLOCKS, demand_sleep};
    use std::time::Duration;

    const BLOCK: usize = 960;
    const CAPACITY: usize = BLOCK * 8;
    const CHANNELS: usize = 2;
    const RATE: u32 = 48000;

    #[test]
    fn empty_ring_waits_minimum() {
        assert_eq!(
            demand_sleep(CAPACITY, CAPACITY, BLOCK, CHANNELS, RATE),
            MIN_FILL_SLEEP
        );
    }

    #[test]
    fn at_target_waits_minimum() {
        let target = BLOCK * TARGET_RING_BLOCKS;
        assert_eq!(
            demand_sleep(CAPACITY, CAPACITY - target, BLOCK, CHANNELS, RATE),
            MIN_FILL_SLEEP
        );
    }

    #[test]
    fn one_block_above_target_waits_one_block_period() {
        let occupied = BLOCK * (TARGET_RING_BLOCKS + 1);
        assert_eq!(
            demand_sleep(CAPACITY, CAPACITY - occupied, BLOCK, CHANNELS, RATE),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn far_above_target_clamps_to_maximum() {
        let occupied = BLOCK * 6;
        assert_eq!(
            demand_sleep(CAPACITY, CAPACITY - occupied, BLOCK, CHANNELS, RATE),
            MAX_FILL_SLEEP
        );
    }

    #[test]
    fn full_ring_waits_maximum() {
        assert_eq!(
            demand_sleep(CAPACITY, 0, BLOCK, CHANNELS, RATE),
            MAX_FILL_SLEEP
        );
        assert_eq!(
            demand_sleep(CAPACITY, BLOCK - 1, BLOCK, CHANNELS, RATE),
            MAX_FILL_SLEEP
        );
    }

    #[test]
    fn degenerate_params_wait_maximum() {
        assert_eq!(
            demand_sleep(CAPACITY, CAPACITY, 0, CHANNELS, RATE),
            MAX_FILL_SLEEP
        );
        assert_eq!(
            demand_sleep(CAPACITY, CAPACITY, BLOCK, 0, RATE),
            MAX_FILL_SLEEP
        );
        assert_eq!(
            demand_sleep(CAPACITY, CAPACITY, BLOCK, CHANNELS, 0),
            MAX_FILL_SLEEP
        );
    }
}
