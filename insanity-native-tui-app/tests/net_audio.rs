use insanity_core::audio_source::SyncAudioSource;
use insanity_native_tui_app::audio::{AudioInputHub, AudioMixer};
use insanity_native_tui_app::audio_test_support::{
    SineSource, energy_ratio, loudness, max_normalized_xcorr,
};
use insanity_native_tui_app::clerver::run_clerver;
use insanity_native_tui_app::protocol::ProtocolMessage;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize},
};
use std::time::Duration;
use tokio::sync::broadcast;
use veq::veq::VeqSocket;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const AUDIO_SECS: u64 = 10;
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const CHUNK: usize = 960;

fn reference(freq: f32, len: usize) -> Vec<f32> {
    let mut src = SineSource::new_amp(48000, 2, freq, 0.5);
    (0..len).map(|_| src.next_sync().expect("sine")).collect()
}

async fn sample_speaker(mixer: &AudioMixer, chunks: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(chunks * CHUNK);
    for _ in 0..chunks {
        let mut buf = vec![0f32; CHUNK];
        mixer.fill_buffer(&mut buf);
        out.extend_from_slice(&buf);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    out
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

        let hub_a = Arc::new(AudioInputHub::from_source(SineSource::new_amp(
            48000, 2, 440.0, 0.5,
        )));
        let hub_b = Arc::new(AudioInputHub::from_source(SineSource::new_amp(
            48000, 2, 880.0, 0.5,
        )));
        let mixer_a = Arc::new(AudioMixer::new_no_device());
        let mixer_b = Arc::new(AudioMixer::new_no_device());
        for (mixer, id) in [(&mixer_a, peer_id), (&mixer_b, peer_id)] {
            mixer.add_peer(
                id,
                Arc::new(AtomicUsize::new(100)),
                Arc::new(AtomicBool::new(false)),
                None,
            );
        }
        let (pm_a, _) = broadcast::channel::<ProtocolMessage>(10);
        let (pm_b, _) = broadcast::channel::<ProtocolMessage>(10);
        let task_a = tokio::spawn(run_clerver(
            session_a,
            None,
            hub_a,
            mixer_a.clone(),
            pm_a.subscribe(),
            peer_id,
        ));
        let task_b = tokio::spawn(run_clerver(
            session_b,
            None,
            hub_b,
            mixer_b.clone(),
            pm_b.subscribe(),
            peer_id,
        ));

        let ticks = (AUDIO_SECS * 100) as usize;
        let (spk_a, spk_b) = tokio::join!(
            sample_speaker(&mixer_a, ticks),
            sample_speaker(&mixer_b, ticks)
        );
        task_a.abort();
        task_b.abort();

        assert_eq!(spk_a.len(), ticks * CHUNK);
        assert_eq!(spk_b.len(), ticks * CHUNK);
        let tail_chunks = 20;
        for (spk, freq, label) in [
            (spk_b, 440.0, "a->b over socket"),
            (spk_a, 880.0, "b->a over socket"),
        ] {
            let tail = &spk[spk.len() - tail_chunks * CHUNK..];
            let mic = reference(freq, tail.len());
            let xcorr = max_normalized_xcorr(tail, &mic, 960);
            assert!(
                xcorr > 0.8,
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
