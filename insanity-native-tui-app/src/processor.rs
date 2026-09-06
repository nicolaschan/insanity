use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use std::sync::{Arc, Mutex};

use cpal::{Sample, SampleRate};
use insanity_core::audio::AudioChunk;
use insanity_core::audio::denoiser::MultiChannelDenoiser;
use insanity_core::audio_source::SyncAudioSource;
use insanity_core::loudness::calculate_loudness;
use insanity_tui_adapter::AppEvent;
use log::error;
use rubato_audio_source::ResampledAudioSource;
use tokio::sync::mpsc::UnboundedSender;

use crate::denoise::nnnoiseless::NnnoiselessDenoiser;
use crate::realtime_buffer::RealTimeBuffer;
use crate::server::RealtimeAudioSource;

pub const AUDIO_CHUNK_SIZE: usize = 480;
pub const AUDIO_CHANNELS: u16 = 2;

pub struct AudioProcessor<'a> {
    enable_denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    denoiser: Mutex<MultiChannelDenoiser<NnnoiselessDenoiser<'a>>>,
    chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
    audio_receiver: Mutex<ResampledAudioSource<RealtimeAudioSource>>,
    app_event_sender: Option<UnboundedSender<AppEvent>>,
    peer_id: String,
    last_sample: Mutex<f32>,
}

impl AudioProcessor<'_> {
    pub fn new(
        enable_denoise: Arc<AtomicBool>,
        volume: Arc<Mutex<usize>>,
        output_sample_rate: SampleRate,
        app_event_sender: Option<UnboundedSender<AppEvent>>,
        peer_id: String,
    ) -> Self {
        let chunk_buffer = Arc::new(Mutex::new(RealTimeBuffer::new(10)));
        let audio_receiver = RealtimeAudioSource::new(chunk_buffer.clone(), 48000, 2);
        let audio_receiver =
            ResampledAudioSource::new(audio_receiver, output_sample_rate.0, AUDIO_CHUNK_SIZE);

        AudioProcessor {
            enable_denoise,
            volume,
            denoiser: Mutex::new(MultiChannelDenoiser::default()),
            audio_receiver: Mutex::new(audio_receiver),
            chunk_buffer,
            app_event_sender,
            peer_id,
            last_sample: Mutex::new(0.0),
        }
    }

    pub fn handle_incoming(&self, mut chunk: AudioChunk) {
        if self.enable_denoise.load(Ordering::Relaxed) {
            let mut denoiser_guard = self.denoiser.lock().unwrap();
            chunk = denoiser_guard.denoise_chunk(&chunk);
        }

        // Adjust volume if necessary
        let volume = { *self.volume.lock().unwrap() };
        if volume != 100 {
            let mut audio_data = chunk.audio_data;
            let a: f32 = 0.2;
            let volume_multiplier = a * ((1.0 + 1.0 / a).powf(volume as f32 / 100.0) - 1.0);
            for sample in audio_data.iter_mut() {
                *sample *= volume_multiplier;
            }
            chunk.audio_data = audio_data;
        }

        if let Some(app_event_sender) = &self.app_event_sender {
            let loudness = calculate_loudness(&chunk.audio_data[..]);
            let loudness_event = AppEvent::Loudness(self.peer_id.clone(), loudness);
            if let Err(e) = app_event_sender.send(loudness_event) {
                error!("Failed to send loudness event: {:?}", e);
            }
        }

        let mut guard = self.chunk_buffer.lock().unwrap();
        guard.set(chunk.sequence_number, chunk);
    }

    pub fn fill_buffer<T: Sample>(&self, to_fill: &mut [T]) {
        let mut last_sample = {
            let last_sample_guard = self.last_sample.lock().unwrap();
            *last_sample_guard
        };

        // LOL this is insane maybe we should use channels or something proper
        for val in to_fill.iter_mut() {
            let mut audio_receiver_guard = self.audio_receiver.lock().unwrap();
            *val = match audio_receiver_guard.next_sync() {
                None => Sample::from(&last_sample), // cry b/c there's no packets
                Some(sample) => {
                    last_sample = sample;
                    Sample::from(&sample)
                }
            };
        }

        let mut last_sample_guard = self.last_sample.lock().unwrap();
        *last_sample_guard = last_sample;
    }
}
