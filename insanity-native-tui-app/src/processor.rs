use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use std::sync::{Arc, Mutex};

use cpal::{Sample, SampleRate};
use insanity_core::audio_source::SyncAudioSource;
use insanity_core::loudness::calculate_loudness;
use insanity_tui_adapter::AppEvent;
use log::error;
use nnnoiseless::DenoiseState;
use rubato_audio_source::ResampledAudioSource;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::realtime_buffer::RealTimeBuffer;
use crate::server::RealtimeAudioSource;

pub const AUDIO_CHUNK_SIZE: usize = 480;
pub const AUDIO_CHANNELS: u16 = 2;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct AudioFormat {
    pub channel_count: u16,
    pub sample_rate: u32,
}

impl AudioFormat {
    pub fn new(channel_count: u16, sample_rate: u32) -> AudioFormat {
        AudioFormat {
            channel_count,
            sample_rate,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub sequence_number: u128,
    pub audio_data: Vec<f32>,
    pub audio_format: AudioFormat,
}

impl AudioChunk {
    pub fn new(
        sequence_number: u128,
        audio_format: AudioFormat,
        audio_data: Vec<f32>,
    ) -> AudioChunk {
        AudioChunk {
            sequence_number,
            audio_data,
            audio_format,
        }
    }
    pub fn to_format(&self, format: AudioFormat) -> AudioChunk {
        AudioChunk {
            sequence_number: self.sequence_number,
            audio_data: self.audio_data.clone(),
            audio_format: format,
        }
    }
}

pub struct MultiChannelDenoiser<'a> {
    channels: u16,
    denoisers: Vec<DenoiseState<'a>>,
}

impl Default for MultiChannelDenoiser<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiChannelDenoiser<'_> {
    pub fn new() -> Self {
        let denoisers: Vec<DenoiseState> = Vec::new();
        MultiChannelDenoiser {
            channels: 0,
            denoisers,
        }
    }

    fn setup_denoisers(&mut self, channels: u16) {
        if channels != self.channels {
            self.denoisers = Vec::new();
            for _ in 0..channels {
                self.denoisers
                    .push(*DenoiseState::from_model(nnnoiseless::RnnModel::default()));
            }
            self.channels = channels;
        }
    }

    pub fn denoise_chunk(&mut self, chunk: &AudioChunk) -> AudioChunk {
        let magic = 32767.0;

        let mut denoised_output: Vec<f32> = Vec::new();

        let channels = chunk.audio_format.channel_count;
        self.setup_denoisers(channels);

        for audio_chunk in chunk
            .audio_data
            .chunks_exact((channels as usize) * DenoiseState::FRAME_SIZE)
        {
            // Audio data for each channel is interleaved
            // Separate it into a buffer for each channel in the raw_audio Vec
            let mut raw_audio: Vec<[f32; DenoiseState::FRAME_SIZE]> = Vec::new();
            for _ in 0..channels {
                raw_audio.push([0.0; DenoiseState::FRAME_SIZE]);
            }
            let mut denoised_audio: Vec<[f32; DenoiseState::FRAME_SIZE]> = Vec::new();
            for (i, val) in audio_chunk.iter().enumerate() {
                raw_audio[i % (channels as usize)][i / (channels as usize)] = *val * magic;
            }

            // Denoise each channel independently
            for i in 0..channels {
                let mut denoiser = self.denoisers.swap_remove(i as usize);
                let mut denoised_audio_buffer = [0.0; DenoiseState::FRAME_SIZE];
                denoiser.process_frame(&mut denoised_audio_buffer, &raw_audio[i as usize]);
                self.denoisers.insert(i as usize, denoiser);
                denoised_audio.insert(i as usize, denoised_audio_buffer);
            }

            // Re-interleave the audio data
            for i in 0..DenoiseState::FRAME_SIZE {
                for c in 0..channels {
                    denoised_output.push(denoised_audio[c as usize][i] / magic);
                }
            }
        }

        AudioChunk::new(
            chunk.sequence_number,
            chunk.audio_format.clone(),
            denoised_output,
        )
    }
}

pub struct AudioProcessor<'a> {
    enable_denoise: Arc<AtomicBool>,
    volume: Arc<Mutex<usize>>,
    denoiser: Mutex<MultiChannelDenoiser<'a>>,
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
            denoiser: Mutex::new(MultiChannelDenoiser::new()),
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
