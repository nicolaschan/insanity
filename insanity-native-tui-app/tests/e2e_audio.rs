//! End-to-end audio mesh tests: virtual insanity programs wired together.
//!
//! Each node is a full pipeline (synthetic mic → hub → Opus → bincode → Opus
//! → mixer → speaker); see `common` for the harness. Assertions are
//! perceptual-with-tolerance: loudness, energy ratio, and delay-tolerant
//! waveform cross-correlation (Opus shifts phase/delay, so naive sample SNR
//! would be brittle).

use insanity_native_tui_app::audio::JITTER_TARGET_CHUNKS;
use insanity_native_tui_app::audio_test_support::{
    VirtualNode, energy_ratio, goertzel_energy, loudness, max_normalized_xcorr, mesh_timeout,
    render_tick, run_mesh, transfer_tick_timeout,
};
use std::collections::HashMap;
use std::time::Duration;

fn pair(freq_a: f32, freq_b: f32) -> HashMap<String, VirtualNode> {
    let mut nodes = HashMap::new();
    let mut a = VirtualNode::new("a", freq_a);
    let mut b = VirtualNode::new("b", freq_b);
    a.add_inbound("b");
    a.add_outbound("b");
    b.add_inbound("a");
    b.add_outbound("a");
    nodes.insert("a".to_string(), a);
    nodes.insert("b".to_string(), b);
    nodes
}

fn edges(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// Tail window skipping startup transients (jitter priming + PLC fade).
fn tail(samples: &[f32], chunks: usize) -> &[f32] {
    &samples[samples.len() - chunks * 960..]
}

#[tokio::test]
async fn two_node_loopback_waveform() {
    let timeout = mesh_timeout(40, 2).saturating_add(Duration::from_secs(10));
    let res = tokio::time::timeout(timeout, async {
        let mut nodes = pair(440.0, 880.0);
        let edge_list = edges(&[("a", "b"), ("b", "a")]);
        run_mesh(&mut nodes, &edge_list, 40).await;

        let (a_mic, b_spk) = (
            nodes["a"].mic_history.clone(),
            nodes["b"].speaker_history.clone(),
        );
        let (b_mic, a_spk) = (
            nodes["b"].mic_history.clone(),
            nodes["a"].speaker_history.clone(),
        );
        for (mic, spk, label) in [(a_mic, b_spk, "a->b"), (b_mic, a_spk, "b->a")] {
            let mic_tail = tail(&mic, 20);
            let spk_tail = tail(&spk, 20);
            // Same number of realtime ticks were rendered on both ends.
            assert_eq!(spk.len(), 40 * 960, "{label}: speaker underruns");
            let xcorr = max_normalized_xcorr(spk_tail, mic_tail, 960);
            assert!(
                xcorr > 0.8,
                "{label}: waveform substantially same, xcorr {xcorr:.3}"
            );
            let dl = (loudness(spk_tail) - loudness(mic_tail)).abs();
            assert!(dl < 0.1, "{label}: loudness drift {dl:.3}");
            let er = energy_ratio(spk_tail, mic_tail);
            assert!(
                (0.3..3.0).contains(&er),
                "{label}: energy ratio {er:.3} out of band"
            );
        }
    })
    .await;
    assert!(
        res.is_ok(),
        "two_node_loopback_waveform timed out after {timeout:?}"
    );
}

#[tokio::test]
async fn three_node_mesh_topology() {
    let timeout = mesh_timeout(40, 6).saturating_add(Duration::from_secs(10));
    let res = tokio::time::timeout(timeout, async {
        let freqs = [("a", 440.0), ("b", 550.0), ("c", 660.0)];
        let mut nodes = HashMap::new();
        for (name, freq) in freqs {
            // Low amp: two peers sum without clipping (clipped sums create
            // intermodulation tones, e.g. 2*550-440=660, that pollute Goertzel).
            nodes.insert(name.to_string(), VirtualNode::with_amp(name, freq, 0.25));
        }
        let edge_list = edges(&[
            ("a", "b"),
            ("b", "a"),
            ("a", "c"),
            ("c", "a"),
            ("b", "c"),
            ("c", "b"),
        ]);
        // Wire every directed edge on both ends.
        for (tx, rx) in edge_list.iter() {
            nodes.get_mut(tx).expect("node").add_outbound(rx);
            nodes.get_mut(rx).expect("node").add_inbound(tx);
        }
        run_mesh(&mut nodes, &edge_list, 40).await;

        // Each speaker must carry its two peers' freqs strongly and its own
        // (never sent back) weakly.
        let tails: HashMap<String, Vec<f32>> = nodes
            .iter()
            .map(|(n, v)| (n.clone(), tail(&v.speaker_history, 20).to_vec()))
            .collect();
        for (name, own) in freqs {
            let spk = &tails[name];
            let peer_power: f64 = freqs
                .iter()
                .filter(|(n, _)| *n != name)
                .map(|(_, f)| goertzel_energy(spk, *f, 48000.0))
                .sum();
            let own_power = goertzel_energy(spk, own, 48000.0);
            assert!(
                peer_power > 10.0 * own_power.max(1e-9),
                "{name}: peers {peer_power:.1} must dominate own-mic echo {own_power:.1}"
            );
        }
    })
    .await;
    assert!(
        res.is_ok(),
        "three_node_mesh_topology timed out after {timeout:?}"
    );
}

#[tokio::test]
async fn mute_gap_honesty() {
    let post_mute_ticks = 10 + JITTER_TARGET_CHUNKS;
    let total_ticks = 20 + post_mute_ticks;
    let timeout = mesh_timeout(total_ticks, 1).saturating_add(Duration::from_secs(10));
    let res = tokio::time::timeout(timeout, async {
        let mut nodes = pair(440.0, 880.0);
        // Only a->b is transferred; b->a stays unwired so b's sender idles.
        let edge_list = edges(&[("a", "b")]);
        run_mesh(&mut nodes, &edge_list, 10).await;
        nodes.get_mut("a").expect("node").set_muted(true);
        run_mesh(&mut nodes, &edge_list, 10).await;
        // The first ~3 mute renders correctly drain buffered jitter audio; only
        // the starved tail must be faded.
        let muted_tail = tail(&nodes["b"].speaker_history, 3).to_vec();
        nodes.get_mut("a").expect("node").set_muted(false);
        run_mesh(&mut nodes, &edge_list, post_mute_ticks).await;

        // No time compression: every tick rendered exactly one chunk.
        assert_eq!(nodes["b"].speaker_history.len(), total_ticks * 960);
        assert!(
            loudness(&muted_tail) < 0.1,
            "muted window must fade, loudness {:.3}",
            loudness(&muted_tail)
        );
        assert!(
            nodes["b"].metrics_snapshot().gap_detected > 0,
            "mute gap must be recorded honestly"
        );
        // Recovery: post-mute tail correlates again.
        let a_tail = tail(&nodes["a"].mic_history, 10).to_vec();
        let b_tail = tail(&nodes["b"].speaker_history, 10).to_vec();
        let xcorr = max_normalized_xcorr(&b_tail, &a_tail, 960);
        assert!(xcorr > 0.8, "post-mute recovery xcorr {xcorr:.3}");
    })
    .await;
    assert!(res.is_ok(), "mute_gap_honesty timed out after {timeout:?}");
}

#[tokio::test]
async fn denoise_parity_on_tonal_content() {
    let timeout = mesh_timeout(40, 2).saturating_add(Duration::from_secs(10));
    let res = tokio::time::timeout(timeout, async {
        // Same sender fans out to a denoise-off and a denoise-on receiver; sine
        // shape must survive both (guards future denoise regressions).
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), VirtualNode::new("a", 440.0));
        nodes.insert("b".to_string(), VirtualNode::new("b", 880.0));
        nodes.insert("c".to_string(), VirtualNode::new("c", 880.0));
        for rx in ["b", "c"] {
            nodes.get_mut("a").expect("node").add_outbound(rx);
        }
        nodes.get_mut("b").expect("node").add_inbound("a");
        // Denoise on for c only: tonal shape must survive both paths.
        nodes
            .get_mut("c")
            .expect("node")
            .add_inbound_denoise("a", true);
        let edge_list = edges(&[("a", "b"), ("a", "c")]);
        run_mesh(&mut nodes, &edge_list, 40).await;
        for rx in ["b", "c"] {
            let mic = tail(&nodes["a"].mic_history.clone(), 20).to_vec();
            // mic_history is shared-send order; each receiver got every chunk.
            let spk = tail(&nodes[rx].speaker_history.clone(), 20).to_vec();
            let xcorr = max_normalized_xcorr(&spk, &mic, 960);
            assert!(xcorr > 0.7, "{rx}: tonal shape survives, xcorr {xcorr:.3}");
        }
        // Single-direction smoke for the transfer helper itself.
        let mut nodes2 = pair(440.0, 880.0);
        assert!(transfer_tick_timeout(&mut nodes2, "a", "b").await);
        render_tick(nodes2.get_mut("b").expect("node"));
        assert_eq!(nodes2["b"].speaker_history.len(), 960);
    })
    .await;
    assert!(
        res.is_ok(),
        "denoise_parity_on_tonal_content timed out after {timeout:?}"
    );
}
