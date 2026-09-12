#[path = "common/audio_math.rs"]
mod audio_math;
#[path = "common/mesh.rs"]
mod mesh;
#[path = "common/sine.rs"]
mod sine;

use audio_math::{energy_ratio, loudness, max_normalized_xcorr, tail};
use insanity_core::audio::mixer::DEFAULT_JITTER_CHUNKS;
use insanity_core::user_input_event::DenoiseSelection;
use mesh::{VirtualNode, mesh_timeout, render_tick, run_mesh, transfer_tick_timeout};
use std::collections::HashMap;
use std::time::Duration;

fn goertzel_energy(samples: &[f32], freq: f32, sr: f32) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq as f64 / sr as f64;
    let (cw, sw) = (w.cos(), w.sin());
    let (mut u1, mut u2) = (0.0f64, 0.0f64);
    for &s in samples.iter() {
        let u0 = s as f64 + 2.0 * cw * u1 - u2;
        u2 = u1;
        u1 = u0;
    }
    let real = u1 * cw - u2;
    let imag = u1 * sw;
    real * real + imag * imag
}

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
        for (tx, rx) in edge_list.iter() {
            nodes.get_mut(tx).expect("node").add_outbound(rx);
            nodes.get_mut(rx).expect("node").add_inbound(tx);
        }
        run_mesh(&mut nodes, &edge_list, 40).await;

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
    let post_mute_ticks = 10 + DEFAULT_JITTER_CHUNKS;
    let total_ticks = 20 + post_mute_ticks;
    let timeout = mesh_timeout(total_ticks, 1).saturating_add(Duration::from_secs(10));
    let res = tokio::time::timeout(timeout, async {
        let mut nodes = pair(440.0, 880.0);
        let edge_list = edges(&[("a", "b")]);
        run_mesh(&mut nodes, &edge_list, 10).await;
        nodes.get_mut("a").expect("node").set_muted(true);
        run_mesh(&mut nodes, &edge_list, 10).await;
        let muted_tail = tail(&nodes["b"].speaker_history, 3).to_vec();
        nodes.get_mut("a").expect("node").set_muted(false);
        run_mesh(&mut nodes, &edge_list, post_mute_ticks).await;

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
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), VirtualNode::new("a", 440.0));
        nodes.insert("b".to_string(), VirtualNode::new("b", 880.0));
        nodes.insert("c".to_string(), VirtualNode::new("c", 880.0));
        for rx in ["b", "c"] {
            nodes.get_mut("a").expect("node").add_outbound(rx);
        }
        nodes.get_mut("b").expect("node").add_inbound("a");
        nodes
            .get_mut("c")
            .expect("node")
            .add_inbound_denoise("a", DenoiseSelection::default());
        let edge_list = edges(&[("a", "b"), ("a", "c")]);
        run_mesh(&mut nodes, &edge_list, 40).await;
        for rx in ["b", "c"] {
            let mic = tail(&nodes["a"].mic_history.clone(), 20).to_vec();
            let spk = tail(&nodes[rx].speaker_history.clone(), 20).to_vec();
            let xcorr = max_normalized_xcorr(&spk, &mic, 960);
            assert!(xcorr > 0.7, "{rx}: tonal shape survives, xcorr {xcorr:.3}");
        }
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
