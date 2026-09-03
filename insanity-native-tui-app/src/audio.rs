use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    Arc, Mutex,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, Sample, SampleFormat, SampleRate, Stream, StreamConfig};
use insanity_core::audio_source::{AudioSource, SyncAudioSource};
use insanity_tui_adapter::AppEvent;
use tokio::sync::{broadcast, mpsc::UnboundedSender};

use crate::processor::{AudioChunk, MultiChannelDenoiser, AUDIO_CHANNELS, AUDIO_CHUNK_SIZE};
use crate::realtime_buffer::RealTimeBuffer;
use insanity_core::loudness::calculate_loudness;
use rubato_audio_source::ResampledAudioSource;

// shared config helpers

pub(crate) fn find_stereo_input(range: cpal::SupportedInputConfigs) -> Option<cpal::SupportedStreamConfigRange> {
    use itertools::Itertools;
    range.into_iter().find_or_last(|x| x.channels() == AUDIO_CHANNELS)
}

pub(crate) fn find_stereo_output(range: cpal::SupportedOutputConfigs) -> Option<cpal::SupportedStreamConfigRange> {
    use itertools::Itertools;
    range.into_iter().find_or_last(|x| x.channels() == AUDIO_CHANNELS)
}

pub(crate) fn get_input_config(device: &Device) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let range = device.supported_input_configs().map_err(|e| anyhow::anyhow!(e))?;
    let cfg_range = find_stereo_input(range).ok_or_else(|| anyhow::anyhow!("No supported input config"))?;
    let max = cfg_range.max_sample_rate();
    let channels = cfg_range.channels();
    let sample_rate = std::cmp::min(SampleRate(48000), max);
    let buffer_size = match cfg_range.buffer_size() {
        cpal::SupportedBufferSize::Range { min: _, max: _ } => BufferSize::Default,
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    };
    let cfg = StreamConfig { channels, sample_rate, buffer_size };
    Ok((cfg_range.sample_format(), cfg))
}

pub(crate) fn get_output_config(device: &Device) -> anyhow::Result<(SampleFormat, StreamConfig)> {
    let range = device.supported_output_configs().map_err(|e| anyhow::anyhow!(e))?;
    let cfg_range = find_stereo_output(range).ok_or_else(|| anyhow::anyhow!("No supported output config"))?;
    let max = cfg_range.max_sample_rate();
    let channels = cfg_range.channels();
    let sample_rate = std::cmp::min(SampleRate(48000), max);
    let buffer_size = match cfg_range.buffer_size() {
        cpal::SupportedBufferSize::Range { min: _, max: _ } => BufferSize::Default,
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    };
    let cfg = StreamConfig { channels, sample_rate, buffer_size };
    Ok((cfg_range.sample_format(), cfg))
}

fn run_input<T: Sample>(config: &StreamConfig, device: &Device, sender: tokio::sync::mpsc::UnboundedSender<f32>) -> Stream {
    let err_fn = |err| eprintln!("input stream error: {err}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for s in data.iter() {
                    let _ = sender.send(s.to_f32());
                }
            },
            err_fn,
        )
        .unwrap()
}

fn setup_input_stream(sample_format: &SampleFormat, config: &StreamConfig, device: &Device, sender: tokio::sync::mpsc::UnboundedSender<f32>) -> Stream {
    match sample_format {
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
    }
}

// RealtimeAudioSource used for output per-peer
pub struct RealtimeAudioSource {
    chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
    sample_buffer: VecDeque<f32>,
    sample_rate: u32,
    channels: u16,
}

impl RealtimeAudioSource {
    pub fn new(chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>, sample_rate: u32, channels: u16) -> Self {
        Self { chunk_buffer, sample_buffer: VecDeque::new(), sample_rate, channels }
    }
}

impl AudioSource for RealtimeAudioSource {
    async fn next(&mut self) -> Option<f32> {
        self.next_sync()
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

impl SyncAudioSource for RealtimeAudioSource {
    fn next_sync(&mut self) -> Option<f32> {
        if self.sample_buffer.is_empty() {
            let mut buf = self.chunk_buffer.lock().unwrap();
            if let Some(chunk) = buf.next_item() {
                self.sample_buffer.extend(chunk.audio_data);
            }
        }
        self.sample_buffer.pop_front()
    }
}

// Cpal receiver for single input
struct CpalStreamReceiver {
    _stream: send_safe::SendWrapperThread<Stream>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<f32>,
    sample_rate: u32,
    channels: u16,
}

impl AudioSource for CpalStreamReceiver {
    async fn next(&mut self) -> Option<f32> {
        self.receiver.recv().await
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

fn make_single_input() -> Option<CpalStreamReceiver> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let Ok((fmt, cfg)) = get_input_config(&device) else {
        log::warn!("Failed to get input config, falling back to silence");
        return None;
    };
    let cfg2 = cfg.clone();
    let mut wrapper = send_safe::SendWrapperThread::new(move || setup_input_stream(&fmt, &cfg2, &device, tx));
    wrapper.execute(|s| s.play().unwrap()).unwrap();
    Some(CpalStreamReceiver { _stream: wrapper, receiver: rx, sample_rate: cfg.sample_rate.0, channels: cfg.channels })
}

// Single input hub

pub struct AudioInputHub {
    tx: broadcast::Sender<Arc<Vec<f32>>>,
    muted: Arc<AtomicBool>,
    channels: u16,
}

impl AudioInputHub {
    pub fn new() -> Self {
        let (btx, _) = broadcast::channel(32);
        let muted = Arc::new(AtomicBool::new(false));
        let muted_clone = muted.clone();
        let btx_clone = btx.clone();

        // Synchronously probe input device to determine correct channel count for the encoder.
        // Fallback to stereo if no device.
        let initial_channels = {
            let host = cpal::default_host();
            if let Some(device) = host.default_input_device() {
                if let Ok((_fmt, cfg)) = get_input_config(&device) {
                    cfg.channels
                } else {
                    AUDIO_CHANNELS
                }
            } else {
                AUDIO_CHANNELS
            }
        };

        // spawn task that captures single input and resamples to 48000
        tokio::spawn(async move {
            let Some(receiver) = make_single_input() else {
                // no input device: send silence periodically so senders don't block forever
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    if muted_clone.load(Ordering::Relaxed) {
                        continue;
                    }
                    let chunk = vec![0.0f32; AUDIO_CHUNK_SIZE * AUDIO_CHANNELS as usize];
                    let _ = btx_clone.send(Arc::new(chunk));
                }
            };
            let channels = receiver.channels;
            let mut resampled = ResampledAudioSource::new(receiver, 48000, AUDIO_CHUNK_SIZE);
            loop {
                let mut chunk = Vec::with_capacity(AUDIO_CHUNK_SIZE * channels as usize);
                for _ in 0..AUDIO_CHUNK_SIZE * channels as usize {
                    if let Some(s) = resampled.next().await {
                        chunk.push(s);
                    } else {
                        return;
                    }
                }
                if muted_clone.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
                let _ = btx_clone.send(Arc::new(chunk));
            }
        });

        Self { tx: btx, muted, channels: initial_channels }
    }

    // test seam: single input from any AudioSource
    pub fn from_source<R>(source: R) -> Self
    where
        R: AudioSource + Send + Sync + 'static,
    {
        let (btx, _) = broadcast::channel(32);
        let muted = Arc::new(AtomicBool::new(false));
        let muted_clone = muted.clone();
        let btx_clone = btx.clone();
        let channels = source.channels();
        tokio::spawn(async move {
            let mut resampled = ResampledAudioSource::new(source, 48000, AUDIO_CHUNK_SIZE);
            loop {
                let mut chunk = Vec::with_capacity(AUDIO_CHUNK_SIZE * channels as usize);
                for _ in 0..AUDIO_CHUNK_SIZE * channels as usize {
                    if let Some(s) = resampled.next().await {
                        chunk.push(s);
                    } else {
                        return;
                    }
                }
                if muted_clone.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
                let _ = btx_clone.send(Arc::new(chunk));
                tokio::task::yield_now().await;
            }
        });
        Self { tx: btx, muted, channels }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Vec<f32>>> {
        self.tx.subscribe()
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

// Output mixer

pub fn volume_multiplier(volume: usize) -> f32 {
    let vol = volume.min(200) as f32;
    let a: f32 = 0.2;
    let m = a * ((1.0 + 1.0 / a).powf(vol / 100.0) - 1.0);
    // cap to avoid inf/NaN before hard clip in mixer; 8.0 (~+18dB) is ample
    m.clamp(0.0, 8.0)
}

struct PeerState {
    chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
    audio_receiver: Mutex<ResampledAudioSource<RealtimeAudioSource>>,
    denoiser: Mutex<MultiChannelDenoiser<'static>>,
    volume: Arc<AtomicUsize>,
    enable_denoise: Arc<AtomicBool>,
    app_event_sender: Option<UnboundedSender<AppEvent>>,
    peer_id: String,
    last_sample: AtomicU32,
}

struct MixerInner {
    peers: HashMap<uuid::Uuid, PeerState>,
}

pub struct AudioMixer {
    inner: Arc<Mutex<MixerInner>>,
    master_volume: Arc<AtomicUsize>,
    _stream: Option<send_safe::SendWrapperThread<Stream>>,
    sample_rate: SampleRate,
    channels: u16,
}

impl AudioMixer {
    pub fn new_no_device() -> Self {
        let inner = Arc::new(Mutex::new(MixerInner { peers: HashMap::new() }));
        let master_volume = Arc::new(AtomicUsize::new(100));
        Self { inner, master_volume, _stream: None, sample_rate: SampleRate(48000), channels: AUDIO_CHANNELS }
    }

    pub fn new(_app_event_sender: Option<UnboundedSender<AppEvent>>) -> Self {
        let host = cpal::default_host();
        // try to get default output device; if none, create dummy mixer without stream
        let inner = Arc::new(Mutex::new(MixerInner { peers: HashMap::new() }));
        let master_volume = Arc::new(AtomicUsize::new(100));
        let inner_clone = inner.clone();
        let master_clone = master_volume.clone();

        let output_device = host.default_output_device();
        let (sample_rate, channels, _stream) = if let Some(device) = output_device {
            match get_output_config(&device) {
                Ok((fmt, cfg)) => {
                    let sr = cfg.sample_rate;
                    let ch = cfg.channels;
                    let cfg2 = cfg.clone();
                    let inner2 = inner_clone.clone();
                    let master2 = master_clone.clone();
                    let mut wrapper = send_safe::SendWrapperThread::new(move || {
                        let inner3 = inner2.clone();
                        let master3 = master2.clone();
                        let err_fn = |err| eprintln!("output stream error: {err}");
                        match fmt {
                            SampleFormat::F32 => device.build_output_stream(
                                &cfg2,
                                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                    fill_buffer_inner(&inner3, &master3, data);
                                },
                                err_fn,
                            ).unwrap(),
                            SampleFormat::I16 => device.build_output_stream(
                                &cfg2,
                                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                                    fill_buffer_inner(&inner3, &master3, data);
                                },
                                err_fn,
                            ).unwrap(),
                            SampleFormat::U16 => device.build_output_stream(
                                &cfg2,
                                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                                    fill_buffer_inner(&inner3, &master3, data);
                                },
                                err_fn,
                            ).unwrap(),
                        }
                    });
                    wrapper.execute(|s| s.play().unwrap()).unwrap();
                    (sr, ch, Some(wrapper))
                }
                Err(e) => {
                    log::warn!("Failed to get output config: {e}, falling back to dummy");
                    (SampleRate(48000), AUDIO_CHANNELS, None)
                }
            }
        } else {
            (SampleRate(48000), AUDIO_CHANNELS, None)
        };

        Self { inner, master_volume, _stream, sample_rate, channels }
    }

    pub fn add_peer(&self, id: uuid::Uuid, volume: Arc<AtomicUsize>, enable_denoise: Arc<AtomicBool>, app_event_sender: Option<UnboundedSender<AppEvent>>) {
        let mut guard = self.inner.lock().unwrap();
        if guard.peers.contains_key(&id) {
            return;
        }
        let chunk_buffer = Arc::new(Mutex::new(RealTimeBuffer::new(10)));
        let audio_receiver = RealtimeAudioSource::new(chunk_buffer.clone(), 48000, AUDIO_CHANNELS);
        let audio_receiver = ResampledAudioSource::new(audio_receiver, self.sample_rate.0, AUDIO_CHUNK_SIZE);
        let state = PeerState {
            chunk_buffer,
            audio_receiver: Mutex::new(audio_receiver),
            denoiser: Mutex::new(MultiChannelDenoiser::new()),
            volume,
            enable_denoise,
            app_event_sender,
            peer_id: id.to_string(),
            last_sample: AtomicU32::new(0.0f32.to_bits()),
        };
        guard.peers.insert(id, state);
    }

    pub fn remove_peer(&self, id: &uuid::Uuid) {
        let mut guard = self.inner.lock().unwrap();
        guard.peers.remove(id);
    }

    pub fn handle_incoming(&self, id: uuid::Uuid, mut chunk: AudioChunk) {
        let mut guard = self.inner.lock().unwrap();
        let Some(peer) = guard.peers.get_mut(&id) else { return };
        // denoise before mixing
        if peer.enable_denoise.load(Ordering::Relaxed) {
            let mut d = peer.denoiser.lock().unwrap();
            chunk = d.denoise_chunk(&chunk);
        }
        let vol = peer.volume.load(Ordering::Relaxed);
        if vol != 100 {
            let m = volume_multiplier(vol);
            for s in chunk.audio_data.iter_mut() {
                *s *= m;
            }
        }
        if let Some(sender) = &peer.app_event_sender {
            let loudness = calculate_loudness(&chunk.audio_data);
            let _ = sender.send(AppEvent::Loudness(peer.peer_id.clone(), loudness));
        }
        let mut buf = peer.chunk_buffer.lock().unwrap();
        buf.set(chunk.sequence_number, chunk);
    }

    pub fn set_master_volume(&self, vol: usize) {
        self.master_volume.store(vol.min(200), Ordering::Relaxed);
    }

    pub fn master_volume(&self) -> usize {
        self.master_volume.load(Ordering::Relaxed)
    }

    pub fn fill_buffer<T: Sample>(&self, data: &mut [T]) {
        fill_buffer_inner(&self.inner, &self.master_volume, data);
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.0
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

fn fill_buffer_inner<T: Sample>(
    inner: &Arc<Mutex<MixerInner>>,
    master_volume: &Arc<AtomicUsize>,
    data: &mut [T],
) {
    let master_vol = master_volume.load(Ordering::Relaxed);
    let master_mult = volume_multiplier(master_vol);
    let use_master = master_vol != 100;

    if inner.lock().unwrap().peers.is_empty() {
        for out in data.iter_mut() {
            *out = Sample::from(&0.0f32);
        }
        return;
    }

    for out in data.iter_mut() {
        let mut mixed: f32 = 0.0;
        // Hold inner only for the sample fetch; drops immediately after.
        let mut guard = inner.lock().unwrap();
        if guard.peers.is_empty() {
            *out = Sample::from(&0.0f32);
            continue;
        }
        for peer in guard.peers.values_mut() {
            let sample_opt = {
                let mut recv = peer.audio_receiver.lock().unwrap();
                recv.next_sync()
            };
            let s = match sample_opt {
                Some(v) => {
                    peer.last_sample.store(v.to_bits(), Ordering::Relaxed);
                    v
                }
                None => f32::from_bits(peer.last_sample.load(Ordering::Relaxed)),
            };
            mixed += s;
        }
        drop(guard);
        if use_master {
            mixed *= master_mult;
        }
        let clipped = mixed.clamp(-1.0, 1.0);
        *out = Sample::from(&clipped);
    }
}
