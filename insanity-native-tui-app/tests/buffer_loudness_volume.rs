use insanity_core::audio_source::{AudioSource, SyncAudioSource};
use insanity_core::loudness::calculate_loudness;
use insanity_native_tui_app::audio::volume_multiplier;
use insanity_native_tui_app::processor::{AUDIO_CHANNELS, AUDIO_CHUNK_SIZE};
use insanity_native_tui_app::realtime_buffer::RealTimeBuffer;
use rubato_audio_source::ResampledAudioSource;

// keep tests simple

#[test]
fn realtime_buffer_basic_order() {
    let mut buf = RealTimeBuffer::new(10);
    for i in 0..5 {
        buf.set(i, i as i32);
    }
    for i in 0..5 {
        assert_eq!(buf.next_item(), Some(i as i32));
    }
    assert_eq!(buf.next_item(), None);
}

#[test]
fn realtime_buffer_gap_and_wrap() {
    let mut buf = RealTimeBuffer::new(3);
    buf.set(0, 0);
    buf.set(2, 2);
    // missing 1, next should skip gap
    assert_eq!(buf.next_item(), Some(0));
    assert_eq!(buf.next_item(), Some(2));
    // duplicate past head ignored
    buf.set(0, 99);
    assert_eq!(buf.next_item(), None);
    // fast forward when too far
    buf.set(10, 10);
    buf.set(11, 11);
    buf.set(12, 12);
    assert_eq!(buf.next_item(), Some(10));
}

#[test]
fn loudness_silent_and_full() {
    assert_eq!(calculate_loudness(&[]), 0.0);
    let silent = vec![0.0f32; 480];
    assert!(calculate_loudness(&silent) < 0.01);
    let full = vec![1.0f32; 480];
    let l = calculate_loudness(&full);
    assert!(l > 0.99, "full scale should be ~1.0 got {l}");
    let half = vec![0.5f32; 480];
    let l_half = calculate_loudness(&half);
    // -6dB from full => ~0.88 on 0..1 scale
    assert!(l_half > 0.8 && l_half < 0.95, "half {l_half}");
}

#[test]
fn volume_curve() {
    assert_eq!(volume_multiplier(0), 0.0);
    assert!((volume_multiplier(100) - 1.0).abs() < 1e-6);
    let v50 = volume_multiplier(50);
    assert!(v50 > 0.2 && v50 < 0.35, "v50 {v50}");
    assert!(volume_multiplier(25) < v50);
    assert!(volume_multiplier(75) > v50);
}

#[test]
fn resampler_passthrough() {
    struct Passthrough {
        data: Vec<f32>,
        pos: usize,
    }
    impl AudioSource for Passthrough {
        async fn next(&mut self) -> Option<f32> {
            if self.pos < self.data.len() { let v = self.data[self.pos]; self.pos+=1; Some(v)} else {None}
        }
        fn sample_rate(&self) -> u32 { 48000 }
        fn channels(&self) -> u16 { 2 }
    }
    impl SyncAudioSource for Passthrough {
        fn next_sync(&mut self) -> Option<f32> {
            if self.pos < self.data.len() { let v = self.data[self.pos]; self.pos+=1; Some(v)} else {None}
        }
    }
    let data: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();
    let src = Passthrough { data: data.clone(), pos: 0 };
    let mut res = ResampledAudioSource::new(src, 48000, AUDIO_CHUNK_SIZE);
    // passthrough should be identical
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let out: Vec<f32> = rt.block_on(async {
        let mut v = Vec::new();
        for _ in 0..960 { v.push(res.next().await.unwrap()); }
        v
    });
    for (a,b) in data.iter().zip(out.iter()) {
        assert!((a-b).abs() < 1e-6);
    }
    assert_eq!(AUDIO_CHUNK_SIZE, 480);
    assert_eq!(AUDIO_CHANNELS, 2);
}
