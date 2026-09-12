use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use insanity_core::audio::AudioFormat;
use insanity_core::audio::codec::EncodedChunk;
use insanity_core::audio::mixer::{Mixer, MixerInput, MixerMetrics, SlotId};
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::{
    ChunkTransform, Denoise, DenoiseControl, Gain, GainControl, Link, MetricsReader, MetricsState,
};
use insanity_core::user_input_event::DenoiseSelection;
use rtrb::Producer;
use rubato_audio_source::StreamResampler;
use tokio::sync::{mpsc, oneshot};

use super::codec::OpusDecoder;
use super::denoise::NnnoiselessDenoiser;
use super::output::{OutputStats, RING_CAPACITY_BLOCKS};
use super::params::{CHUNK_PERIOD, MAX_VOLUME, SAMPLE_RATE};

pub type PeerChain = Link<Denoise<NnnoiselessDenoiser>, Link<Gain, MetricsReader>>;
pub(crate) type OpusRebuild = fn(&AudioFormat) -> Option<OpusDecoder>;
pub type AppMixer = Mixer<OpusDecoder, PeerChain, StreamResampler, Gain, OpusRebuild>;

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

pub fn chain_from_controls(controls: &PeerControls) -> PeerChain {
    Denoise::new(controls.denoise.clone()).chain(
        Gain::new(controls.gain.clone()).chain(MetricsReader::new(controls.loudness.clone())),
    )
}

pub fn rebuild_opus_decoder(format: &AudioFormat) -> Option<OpusDecoder> {
    OpusDecoder::new(format.sample_rate, format.channel_count)
}

pub fn output_resampler(out: AudioFormat, block_frames: usize) -> StreamResampler {
    StreamResampler::new(
        AudioFormat::new(out.channel_count, SAMPLE_RATE),
        out.sample_rate,
        block_frames,
    )
}

pub(crate) const MIXER_OPS_BOUND: usize = 64;

pub(crate) enum MixerOp {
    Push {
        slot: SlotId,
        chunk: EncodedChunk,
    },
    Subscribe {
        transform: PeerChain,
        decoder: OpusRebuild,
        resampler: StreamResampler,
        reply: oneshot::Sender<SlotId>,
    },
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
        resampler: StreamResampler,
    ) -> Option<SlotId> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MixerOp::Subscribe {
                transform,
                decoder,
                resampler,
                reply: reply_tx,
            })
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

impl MixerInput<OpusDecoder, PeerChain, StreamResampler, OpusRebuild> for MixerClient {
    fn push_frame(&mut self, slot: SlotId, frame: EncodedChunk) -> bool {
        MixerClient::push_frame(self, slot, frame)
    }

    async fn subscribe(
        &mut self,
        transform: PeerChain,
        rebuild: OpusRebuild,
        resampler: StreamResampler,
    ) -> Option<SlotId> {
        MixerClient::subscribe(self, transform, rebuild, resampler).await
    }

    async fn unsubscribe(&mut self, slot: SlotId) {
        MixerClient::unsubscribe(self, slot).await;
    }

    async fn snapshot(&self) -> Option<(MixerMetrics, usize)> {
        MixerClient::snapshot(self).await
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

pub(crate) async fn run_mixer_owner(
    mut mixer: AppMixer,
    mut ring: Producer<f32>,
    stats: Arc<OutputStats>,
    mut rx: mpsc::Receiver<MixerOp>,
    block_samples: usize,
) {
    let mut batch = Vec::with_capacity(MIXER_OPS_BOUND);
    let mut ticker = tokio::time::interval(CHUNK_PERIOD);
    let mut block = Vec::with_capacity(block_samples.max(1));
    loop {
        tokio::select! {
            biased;
            count = rx.recv_many(&mut batch, MIXER_OPS_BOUND) => {
                if count == 0 {
                    break;
                }
            }
            _ = ticker.tick() => {}
        }
        let mut slot_replies = Vec::new();
        let mut snapshot_replies = Vec::new();
        for op in batch.drain(..) {
            match op {
                MixerOp::Push { slot, chunk } => {
                    mixer.push_to_slot(slot, chunk);
                }
                MixerOp::Subscribe {
                    transform,
                    decoder,
                    resampler,
                    reply,
                } => {
                    let slot = mixer.subscribe(transform, decoder, resampler);
                    slot_replies.push((reply, slot));
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
        for _ in 0..RING_CAPACITY_BLOCKS {
            if ring.slots() < block_samples {
                break;
            }
            block.extend((0..block_samples).map(|_| mixer.next_sync().unwrap_or(0.0)));
            let mut overruns = 0;
            for sample in block.drain(..) {
                if ring.push(sample).is_err() {
                    overruns += 1;
                }
            }
            if overruns > 0 {
                stats.note_overrun(overruns);
            }
        }
    }
}
