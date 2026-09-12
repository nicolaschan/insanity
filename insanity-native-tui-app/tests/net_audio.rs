#[path = "common/audio_math.rs"]
mod audio_math;
#[path = "common/sine.rs"]
mod sine;

use audio_math::{energy_ratio, loudness, max_normalized_xcorr};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::codec::EncodedChunk;
use insanity_core::audio::mixer::{DEFAULT_OUT_FRAMES, SlotId};
use insanity_core::audio::sample::SyncSampleSource;
use insanity_core::audio::transform::MetricsState;
use insanity_core::user_input_event::DenoiseSelection;
use insanity_native_tui_app::audio::{
    AppMixer, PeerControls, chain_from_controls, output_resampler, rebuild_opus_decoder,
};
use insanity_native_tui_app::clerver::run_clerver;
use insanity_native_tui_app::processor::AUDIO_SAMPLE_RATE;
use insanity_native_tui_app::protocol::ProtocolMessage;
use sine::{SineSource, hub_from_source, new_no_device_mixer};
use std::future::Ready;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use veq::veq::VeqSocket;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const AUDIO_SECS: u64 = 10;
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const CHUNK: usize = 960;

async fn sample_speaker(mixer: &Arc<Mutex<AppMixer>>, chunks: usize) -> (Vec<f32>, u64) {
    let mut out = Vec::with_capacity(chunks * CHUNK);
    let mut total_nanos: u64 = 0;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(10));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    for _ in 0..chunks {
        ticker.tick().await;
        let mut buf = vec![0f32; CHUNK];
        let start = std::time::Instant::now();
        {
            let mut guard = mixer.lock().expect("mixer lock");
            for sample in buf.iter_mut() {
                *sample = guard.next_sync().unwrap_or(0.0);
            }
        }
        total_nanos += start.elapsed().as_nanos() as u64;
        out.extend_from_slice(&buf);
    }
    let avg = total_nanos / (chunks as u64).max(1);
    (out, avg)
}

fn subscribe_peer(mixer: &Arc<Mutex<AppMixer>>) -> (SlotId, Arc<MetricsState>) {
    let controls = PeerControls::new(100, DenoiseSelection::None);
    let chain = chain_from_controls(&controls);
    let mut guard = mixer.lock().expect("mixer lock");
    let slot = guard.subscribe(
        chain,
        rebuild_opus_decoder,
        output_resampler(AudioFormat::new(2, AUDIO_SAMPLE_RATE), DEFAULT_OUT_FRAMES),
    );
    (slot, controls.loudness.clone())
}

fn push_to(mixer: Arc<Mutex<AppMixer>>, slot: SlotId) -> impl FnMut(EncodedChunk) -> Ready<bool> {
    move |frame: EncodedChunk| {
        let mut guard = mixer.lock().expect("mixer lock");
        let accepted = guard.push_to_slot(slot, frame);
        std::future::ready(accepted)
    }
}

#[tokio::test]
async fn connected_peers_exchange_audio() {
    let res = tokio::time::timeout(TEST_TIMEOUT, async {
        let mut socket_a = VeqSocket::bind("127.0.0.1:0").await.expect("bind a");
        let mut socket_b = VeqSocket::bind("127.0.0.1:0").await.expect("bind b");
        let (info_a, info_b) = (socket_a.connection_info(), socket_b.connection_info());
        let peer_id = uuid::Uuid::new_v4();
        let (session_a, session_b) = tokio::time::timeout(CONNECT_TIMEOUT, async {
            tokio::join!(
                socket_a.connect(peer_id, info_b),
                socket_b.connect(peer_id, info_a)
            )
        })
        .await
        .expect("connect timed out");
        let (session_a, session_b) = (session_a.expect("connect a"), session_b.expect("connect b"));

        let hub_a = Arc::new(hub_from_source(
            SineSource::new_amp(48000, 440.0, 0.5),
            AudioFormat::new(2, 48000),
        ));
        let hub_b = Arc::new(hub_from_source(
            SineSource::new_amp(48000, 880.0, 0.5),
            AudioFormat::new(2, 48000),
        ));
        let mixer_a: Arc<Mutex<AppMixer>> = Arc::new(Mutex::new(new_no_device_mixer()));
        let mixer_b: Arc<Mutex<AppMixer>> = Arc::new(Mutex::new(new_no_device_mixer()));
        let (slot_a, loudness_a) = subscribe_peer(&mixer_a);
        let (slot_b, loudness_b) = subscribe_peer(&mixer_b);
        let (pm_a, _) = broadcast::channel::<ProtocolMessage>(10);
        let (pm_b, _) = broadcast::channel::<ProtocolMessage>(10);
        let task_a = tokio::spawn(run_clerver(
            session_a,
            None,
            hub_a,
            push_to(mixer_a.clone(), slot_a),
            loudness_a,
            peer_id.to_string(),
            pm_a.subscribe(),
        ));
        let task_b = tokio::spawn(run_clerver(
            session_b,
            None,
            hub_b,
            push_to(mixer_b.clone(), slot_b),
            loudness_b,
            peer_id.to_string(),
            pm_b.subscribe(),
        ));

        let ticks = (AUDIO_SECS * 100) as usize;
        let started = std::time::Instant::now();
        let ((spk_a, avg_a), (spk_b, avg_b)) = tokio::join!(
            sample_speaker(&mixer_a, ticks),
            sample_speaker(&mixer_b, ticks)
        );
        task_a.abort();
        task_b.abort();
        eprintln!(
            "sampled {ticks} ticks in {:?}; mixer_a {:?} fill_avg {avg_a}ns; mixer_b {:?} fill_avg {avg_b}ns",
            started.elapsed(),
            mixer_a.lock().expect("lock").metrics_snapshot(),
            mixer_b.lock().expect("lock").metrics_snapshot(),
        );

        assert_eq!(spk_a.len(), ticks * CHUNK);
        assert_eq!(spk_b.len(), ticks * CHUNK);
        let tail_chunks = 20;
        for (spk, freq, label) in [
            (spk_b, 440.0, "a->b over socket"),
            (spk_a, 880.0, "b->a over socket"),
        ] {
            let tail = &spk[spk.len() - tail_chunks * CHUNK..];
            let mic = SineSource::reference(freq, tail.len());
            let xcorr = max_normalized_xcorr(tail, &mic, 960);
            assert!(
                xcorr > 0.7,
                "{label}: waveform substantially same, xcorr {xcorr:.3}"
            );
            let dl = (loudness(tail) - loudness(&mic)).abs();
            assert!(dl < 0.1, "{label}: loudness drift {dl:.3}");
            let er = energy_ratio(tail, &mic);
            assert!(
                (0.3..3.0).contains(&er),
                "{label}: energy ratio {er:.3} out of band"
            );
        }
    })
    .await;
    assert!(
        res.is_ok(),
        "connected_peers_exchange_audio timed out after {TEST_TIMEOUT:?}"
    );
}
