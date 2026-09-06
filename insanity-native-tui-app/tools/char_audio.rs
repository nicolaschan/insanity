use insanity_core::audio_source::{AudioSource, SyncAudioSource};
use insanity_native_tui_app::audio_test_support::{
    VirtualNode, energy_ratio, goertzel_energy, loudness, max_normalized_xcorr, render_tick,
    run_mesh, transfer_tick_timeout,
};
use std::collections::HashMap;
use std::path::PathBuf;

struct ChirpSource {
    n: u64,
    sr: u32,
    ch: u16,
    amp: f32,
    total: u64,
    phase: f64,
}

impl ChirpSource {
    fn new(sr: u32, ch: u16, amp: f32, total: u64) -> Self {
        Self {
            n: 0,
            sr,
            ch,
            amp,
            total,
            phase: 0.0,
        }
    }
    fn step(&mut self) -> f32 {
        let t = self.n as f64 / self.total.max(1) as f64;
        let freq = 200.0 + 1800.0 * t;
        self.phase += freq / self.sr as f64;
        self.n += 1;
        ((self.phase * 2.0 * std::f64::consts::PI).sin() as f32) * self.amp
    }
}

impl AudioSource for ChirpSource {
    async fn next(&mut self) -> Option<f32> {
        Some(self.step())
    }
    fn sample_rate(&self) -> u32 {
        self.sr
    }
    fn channels(&self) -> u16 {
        self.ch
    }
}

impl SyncAudioSource for ChirpSource {
    fn next_sync(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

struct AmSpeechSource {
    n: u64,
    sr: u32,
    ch: u16,
    amp: f32,
    phase_a: f64,
    phase_b: f64,
}

impl AmSpeechSource {
    fn new(sr: u32, ch: u16, amp: f32) -> Self {
        Self {
            n: 0,
            sr,
            ch,
            amp,
            phase_a: 0.0,
            phase_b: 0.0,
        }
    }
    fn step(&mut self) -> f32 {
        let t = self.n as f64 / self.sr as f64;
        self.phase_a += 440.0 / self.sr as f64;
        self.phase_b += 880.0 / self.sr as f64;
        self.n += 1;
        let am = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
        let tick = (self.n / 960) % 50;
        let gate = if tick >= 45 { 0.0 } else { 1.0 };
        let v = (self.phase_a * 2.0 * std::f64::consts::PI).sin() * 0.7
            + (self.phase_b * 2.0 * std::f64::consts::PI).sin() * 0.3;
        (v as f32) * (am as f32) * (self.amp / 0.5) * (gate as f32) * 0.5
    }
}

impl AudioSource for AmSpeechSource {
    async fn next(&mut self) -> Option<f32> {
        Some(self.step())
    }
    fn sample_rate(&self) -> u32 {
        self.sr
    }
    fn channels(&self) -> u16 {
        self.ch
    }
}

impl SyncAudioSource for AmSpeechSource {
    fn next_sync(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

struct NoiseSource {
    state: u64,
    sr: u32,
    ch: u16,
    amp: f32,
}

impl NoiseSource {
    fn new(sr: u32, ch: u16, amp: f32, seed: u64) -> Self {
        Self {
            state: seed,
            sr,
            ch,
            amp,
        }
    }
    fn step(&mut self) -> f32 {
        self.state = self.state.wrapping_mul(1664525).wrapping_add(1013904223);
        let u = ((self.state >> 16) & 0xFFFF) as f32 / 65535.0;
        (u * 2.0 - 1.0) * self.amp
    }
}

impl AudioSource for NoiseSource {
    async fn next(&mut self) -> Option<f32> {
        Some(self.step())
    }
    fn sample_rate(&self) -> u32 {
        self.sr
    }
    fn channels(&self) -> u16 {
        self.ch
    }
}

impl SyncAudioSource for NoiseSource {
    fn next_sync(&mut self) -> Option<f32> {
        Some(self.step())
    }
}

struct SilenceSource {
    sr: u32,
    ch: u16,
}

impl AudioSource for SilenceSource {
    async fn next(&mut self) -> Option<f32> {
        Some(0.0)
    }
    fn sample_rate(&self) -> u32 {
        self.sr
    }
    fn channels(&self) -> u16 {
        self.ch
    }
}

impl SyncAudioSource for SilenceSource {
    fn next_sync(&mut self) -> Option<f32> {
        Some(0.0)
    }
}

fn lcg_next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

fn tail_vec(v: &[f32], chunks: usize) -> Vec<f32> {
    let want = chunks * 960;
    if v.len() <= want {
        v.to_vec()
    } else {
        v[v.len() - want..].to_vec()
    }
}

fn gen_sine(len: usize, freq: f32, amp: f32) -> Vec<f32> {
    let mut phase: f32 = 0.0;
    (0..len)
        .map(|_| {
            let v = (phase * 2.0 * std::f32::consts::PI).sin() * amp;
            phase = (phase + freq / 48000.0) % 1.0;
            v
        })
        .collect()
}

fn make_sender(signal: &str, seed: u64) -> VirtualNode {
    match signal {
        "sine440_05" => VirtualNode::new("a", 440.0),
        "sine880_05" => VirtualNode::new("a", 880.0),
        "sine440_025" => VirtualNode::with_amp("a", 440.0, 0.25),
        "chirp" => VirtualNode::with_source("a", ChirpSource::new(48000, 2, 0.4, 40 * 960)),
        "amspeech" => VirtualNode::with_source("a", AmSpeechSource::new(48000, 2, 0.5)),
        "noise" => VirtualNode::with_source("a", NoiseSource::new(48000, 2, 0.4, seed)),
        "silence" => VirtualNode::with_source("a", SilenceSource { sr: 48000, ch: 2 }),
        _ => VirtualNode::with_amp("a", 440.0, 0.5),
    }
}

fn make_pair(signal: &str, seed: u64) -> HashMap<String, VirtualNode> {
    let mut nodes = HashMap::new();
    let mut a = make_sender(signal, seed);
    let mut b = VirtualNode::with_source("b", SilenceSource { sr: 48000, ch: 2 });
    a.add_outbound("b");
    b.add_inbound("a");
    nodes.insert("a".to_string(), a);
    nodes.insert("b".to_string(), b);
    nodes
}

struct CellResult {
    signal: String,
    condition: String,
    run: usize,
    ok: usize,
    total: usize,
    mic_len: usize,
    spk_len: usize,
    xcorr_pos: f64,
    xcorr_neg880: f64,
    d_loud: f64,
    loud_mic: f64,
    loud_spk: f64,
    energy: f64,
    goertzel_440: f64,
    goertzel_880: f64,
    gap: usize,
    underrun: usize,
}

async fn run_cell(signal: &str, condition: &str, run: usize, ticks: usize) -> CellResult {
    let seed = (run as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15);
    let mut nodes = make_pair(signal, seed);
    let mut ok = 0usize;
    let mut rng = seed ^ 0x12345678;
    let mut pending: Option<Vec<u8>> = None;
    let burst_start = ticks / 2;
    let burst_end = burst_start + 3;
    if condition == "mutegap" {
        for t in 0..ticks {
            if t == 10 {
                nodes.get_mut("a").expect("node").set_muted(true);
            }
            if t == 20 {
                nodes.get_mut("a").expect("node").set_muted(false);
            }
            if transfer_tick_timeout(&mut nodes, "a", "b").await {
                ok += 1;
            }
            for name in ["a".to_string(), "b".to_string()] {
                render_tick(nodes.get_mut(&name).expect("node"));
            }
        }
    } else if condition == "delay1" {
        for _ in 0..ticks {
            let fresh = nodes.get_mut("a").expect("node").pull_frame("b").await;
            if let Some(old) = pending.take()
                && nodes
                    .get_mut("b")
                    .expect("node")
                    .push_frame("a", &old)
                    .await
            {
                ok += 1;
            }
            pending = fresh;
            for name in ["a".to_string(), "b".to_string()] {
                render_tick(nodes.get_mut(&name).expect("node"));
            }
        }
        if let Some(old) = pending.take()
            && nodes
                .get_mut("b")
                .expect("node")
                .push_frame("a", &old)
                .await
        {
            ok += 1;
        }
    } else if condition == "clean" {
        run_mesh(&mut nodes, &[("a".to_string(), "b".to_string())], ticks).await;
        // Note: unlike the degradation branches below (which count
        // successful transfers), `ok` here counts mic chunks produced.
        ok = nodes["a"].mic_history.len() / 960;
    } else {
        for t in 0..ticks {
            let drop_this = match condition {
                "clean" => false,
                "drop05" => lcg_next(&mut rng) % 100 < 5,
                "drop20" => lcg_next(&mut rng) % 100 < 20,
                "burst3" => t >= burst_start && t < burst_end,
                "duplicate" => false,
                _ => false,
            };
            if condition == "duplicate" {
                let bytes = nodes.get_mut("a").expect("node").pull_frame("b").await;
                if let Some(b) = bytes {
                    let r1 = nodes.get_mut("b").expect("node").push_frame("a", &b).await;
                    let _ = nodes.get_mut("b").expect("node").push_frame("a", &b).await;
                    if r1 {
                        ok += 1;
                    }
                }
            } else if drop_this {
                let _ = nodes.get_mut("a").expect("node").pull_frame("b").await;
            } else if transfer_tick_timeout(&mut nodes, "a", "b").await {
                ok += 1;
            }
            for name in ["a".to_string(), "b".to_string()] {
                render_tick(nodes.get_mut(&name).expect("node"));
            }
        }
    }
    let mic = nodes["a"].mic_history.clone();
    let spk = nodes["b"].speaker_history.clone();
    let mic_tail = tail_vec(&mic, 20);
    let spk_tail = tail_vec(&spk, 20);
    let n = mic_tail.len().min(spk_tail.len());
    let mic_tail = mic_tail[mic_tail.len() - n..].to_vec();
    let spk_tail = spk_tail[spk_tail.len() - n..].to_vec();
    let neg880 = gen_sine(n, 880.0, 0.5);
    let e_mic: f64 = mic_tail.iter().map(|v| (*v as f64).powi(2)).sum();
    let (xcorr_pos, xcorr_neg880, energy, d_loud, loud_mic, loud_spk) = if e_mic < 1e-9 || n == 0 {
        (0.0, 0.0, 0.0, 0.0, loudness(&mic_tail), loudness(&spk_tail))
    } else {
        (
            max_normalized_xcorr(&spk_tail, &mic_tail, 960),
            max_normalized_xcorr(&spk_tail, &neg880, 960),
            energy_ratio(&spk_tail, &mic_tail),
            (loudness(&spk_tail) - loudness(&mic_tail)).abs(),
            loudness(&mic_tail),
            loudness(&spk_tail),
        )
    };
    let snap = nodes["b"].metrics_snapshot();
    CellResult {
        signal: signal.to_string(),
        condition: condition.to_string(),
        run,
        ok,
        total: ticks,
        mic_len: mic.len(),
        spk_len: spk.len(),
        xcorr_pos,
        xcorr_neg880,
        d_loud,
        loud_mic,
        loud_spk,
        energy,
        goertzel_440: goertzel_energy(&spk_tail, 440.0, 48000.0),
        goertzel_880: goertzel_energy(&spk_tail, 880.0, 48000.0),
        gap: snap.gap_detected,
        underrun: snap.underrun,
    }
}

fn target_csv() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/char_audio/runs.csv")
}

const CELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CSV_HEADER: &str = "signal,condition,run,ok,total,mic_len,spk_len,xcorr_pos,xcorr_neg880,d_loud,loud_mic,loud_spk,energy,goertzel_440,goertzel_880,gap,underrun\n";

async fn run_cell_timeout(signal: &str, condition: &str, run: usize, ticks: usize) -> CellResult {
    match tokio::time::timeout(CELL_TIMEOUT, run_cell(signal, condition, run, ticks)).await {
        Ok(cell) => cell,
        Err(_) => {
            panic!("run_cell({signal},{condition},{run},{ticks}) timed out after {CELL_TIMEOUT:?}")
        }
    }
}

fn format_row(r: &CellResult) -> String {
    format!(
        "{},{},{},{},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.1},{:.1},{},{}\n",
        r.signal,
        r.condition,
        r.run,
        r.ok,
        r.total,
        r.mic_len,
        r.spk_len,
        r.xcorr_pos,
        r.xcorr_neg880,
        r.d_loud,
        r.loud_mic,
        r.loud_spk,
        r.energy,
        r.goertzel_440,
        r.goertzel_880,
        r.gap,
        r.underrun
    )
}

fn append_row(path: &std::path::Path, row: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open csv");
    f.write_all(row.as_bytes()).expect("append csv");
}

const CLEAN_SIGNALS: [&str; 7] = [
    "sine440_05",
    "sine880_05",
    "sine440_025",
    "chirp",
    "amspeech",
    "noise",
    "silence",
];
const DEGRADED_SIGNALS: [&str; 4] = ["sine440_05", "chirp", "amspeech", "noise"];
const DEGRADED_CONDITIONS: [&str; 5] = ["drop05", "drop20", "burst3", "delay1", "duplicate"];

fn selected(var: &str, all: &[&str]) -> Vec<String> {
    let raw = std::env::var(var).unwrap_or_default();
    if raw.trim().is_empty() {
        return all.iter().map(|s| s.to_string()).collect();
    }
    let want: Vec<String> = raw.split(',').map(|s| s.trim().to_string()).collect();
    all.iter()
        .filter(|s| want.iter().any(|w| w == *s))
        .map(|s| s.to_string())
        .collect()
}

fn reps() -> usize {
    std::env::var("CHAR_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "usage: char_audio [--help]\n\
             env: CHAR_SIGNALS (default: all of sine440_05,sine880_05,sine440_025,chirp,amspeech,noise,silence)\n\
             env: CHAR_CONDITIONS (default: clean,drop05,drop20,burst3,delay1,duplicate,mutegap)\n\
             env: CHAR_REPS (default: 3)\n\
             writes CSV to <manifest-dir>/../target/char_audio/runs.csv; exit 2 on empty plan, 1 on timeout"
        );
        return;
    }
    let path = target_csv();
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let reps = reps();
    let clean_signals = selected("CHAR_SIGNALS", &CLEAN_SIGNALS);
    let conditions = selected(
        "CHAR_CONDITIONS",
        &[
            "clean",
            "drop05",
            "drop20",
            "burst3",
            "delay1",
            "duplicate",
            "mutegap",
        ],
    );
    let degraded_signals: Vec<String> = clean_signals
        .iter()
        .filter(|s| DEGRADED_SIGNALS.contains(&s.as_str()))
        .cloned()
        .collect();
    let degraded_conditions: Vec<String> = conditions
        .iter()
        .filter(|c| DEGRADED_CONDITIONS.contains(&c.as_str()))
        .cloned()
        .collect();
    let mut plan: Vec<(String, String, usize, usize)> = Vec::new();
    if conditions.iter().any(|c| c == "clean") {
        for s in clean_signals.iter() {
            for r in 0..reps {
                plan.push((s.clone(), "clean".to_string(), r, 40));
            }
        }
    }
    for s in degraded_signals.iter() {
        for c in degraded_conditions.iter() {
            for r in 0..reps {
                plan.push((s.clone(), c.clone(), r, 40));
            }
        }
    }
    if conditions.iter().any(|c| c == "mutegap") && clean_signals.iter().any(|s| s == "sine440_05")
    {
        for r in 0..reps {
            plan.push(("sine440_05".to_string(), "mutegap".to_string(), r, 30));
        }
    }
    if plan.is_empty() {
        eprintln!("char_audio: empty plan; check CHAR_SIGNALS/CHAR_CONDITIONS");
        std::process::exit(2);
    }
    let overall = std::time::Duration::from_secs(plan.len() as u64 * 35 + 60);
    eprintln!(
        "char_audio: {} cells, overall timeout {overall:?}",
        plan.len()
    );
    std::fs::write(&path, CSV_HEADER).expect("write csv header");
    let total = plan.len();
    let res = tokio::time::timeout(overall, async {
        let mut done = 0usize;
        for (signal, condition, run, ticks) in plan {
            let cell = run_cell_timeout(&signal, &condition, run, ticks).await;
            append_row(&path, &format_row(&cell));
            done += 1;
            eprintln!("char_audio: {done}/{total} {signal}/{condition}/{run} ok");
        }
        done
    })
    .await;
    match res {
        Ok(done) => eprintln!("char_audio: done {done}/{total} -> {}", path.display()),
        Err(_) => {
            append_row(&path, "STALLED,timeout,0,0,0,0,0,0,0,0,0,0,0,0,0,0\n");
            eprintln!("char_audio: timed out after {overall:?}; partial rows kept");
            std::process::exit(1);
        }
    }
}
