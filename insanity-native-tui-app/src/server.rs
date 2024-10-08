use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, Sample, SampleFormat, SampleRate, Stream, StreamConfig};
use insanity_core::audio_source::{AudioSource, SyncAudioSource};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::processor::{AudioChunk, AUDIO_CHANNELS};
use crate::realtime_buffer::RealTimeBuffer;

fn run_input<T: Sample>(
    config: &cpal::StreamConfig,
    device: &Device,
    sender: UnboundedSender<f32>,
) -> Stream {
    let err_fn = |err| eprintln!("an error occurred in the input audio stream: {err}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for sample in data.iter() {
                    if let Ok(()) = sender.send(sample.to_f32()) {}
                }
            },
            err_fn,
        )
        .unwrap()
}

fn setup_input_stream(
    sample_format: &SampleFormat,
    config: &cpal::StreamConfig,
    device: &Device,
    sender: UnboundedSender<f32>,
) -> Stream {
    match sample_format {
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
    }
}

fn get_input_config(device: &Device) -> (SampleFormat, cpal::StreamConfig) {
    let supported_configs_range = device.supported_input_configs().unwrap();
    let supported_config_range = find_stereo_input(supported_configs_range).unwrap();
    let max_sample_rate = supported_config_range.max_sample_rate();

    let channels = supported_config_range.channels();
    let sample_rate = std::cmp::min(SampleRate(48000), max_sample_rate);
    let buffer_size = match supported_config_range.buffer_size() {
        cpal::SupportedBufferSize::Range { min: _, max: _ } => BufferSize::Default,
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    };
    // println!("buffer size: {:?}", buffer_size);
    let supported_config = StreamConfig {
        channels,
        sample_rate,
        buffer_size,
    };

    // let supported_config = supported_config_range.with_sample_rate(std::cmp::min(SampleRate(48000), max_sample_rate));
    let sample_format = supported_config_range.sample_format();
    (sample_format, supported_config)
}

fn find_stereo_input(
    range: cpal::SupportedInputConfigs,
) -> Option<cpal::SupportedStreamConfigRange> {
    let mut something = None;
    for item in range {
        if item.channels() == AUDIO_CHANNELS {
            return Some(item);
        } else {
            something = Some(item);
        }
    }
    something
}

pub struct CpalStreamReceiver {
    #[allow(dead_code)]
    input_stream: send_safe::SendWrapperThread<Stream>,
    input_receiver: UnboundedReceiver<f32>,
    sample_rate: u32,
    channels: u16,
}

impl AudioSource for CpalStreamReceiver {
    async fn next(&mut self) -> Option<f32> {
        self.input_receiver.recv().await
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

// impl AudioReceiver for Receiver<f32> {
//     fn receiver(&mut self) -> &mut Receiver<f32> {
//         self
//     }
// }

pub fn make_audio_receiver() -> CpalStreamReceiver {
    let host = cpal::default_host();
    let (input_sender, input_receiver) = unbounded_channel();
    let input_device = host
        .default_input_device()
        .expect("No default input device");
    // If input_stream is dropped, then the input_receiver stops receiving data.
    // CpalStreamReceiver keeps input_stream alive along with input_receiver.
    let (sample_format, config) = get_input_config(&input_device);
    let config_clone = config.clone();
    let mut wrapper = send_safe::SendWrapperThread::new(move || {
        setup_input_stream(&sample_format, &config_clone, &input_device, input_sender)
    });
    wrapper
        .execute(|input_stream| {
            input_stream.play().unwrap();
        })
        .unwrap();
    CpalStreamReceiver {
        input_receiver,
        input_stream: wrapper,
        sample_rate: config.sample_rate.0,
        channels: config.channels,
    }
}

// fn make_music_receiver(path: String) -> Receiver<f32> {
//     let (input_sender, input_receiver) = unbounded();
//     thread::spawn(move || {
//         let mut file = File::open(path).expect("Could not open sound file");
//         let (_, data) = wav::read(&mut file).expect("Could not read sound (wav file)");
//         // println!("Music: {:?}", header);
//         if let Sixteen(vec) = data {
//             let mut now = SystemTime::now();
//             for chunk in vec.chunks_exact(AUDIO_CHUNK_SIZE * (AUDIO_CHANNELS as usize)) {
//                 for val in chunk {
//                     let s: i16 = Sample::from(val);
//                     if input_sender.send(s.to_f32()).is_ok() {}
//                 }
//                 while now.elapsed().unwrap()
//                     < Duration::from_millis(((AUDIO_CHUNK_SIZE * 1000) / 48000).try_into().unwrap())
//                 {
//                     std::hint::spin_loop();
//                 }
//                 now = SystemTime::now()
//             }
//         }
//     });
//     input_receiver
// }

// #[tokio::main]
// pub async fn start_server(socket: VeqSocket, config: InsanityConfig) {
//     if let Some(path) = config.music.clone() {
//         start_server_with_receiver(socket, move || make_music_receiver(path), config).await;
//     } else {
//         let config_clone = config.clone();
//         start_server_with_receiver(socket, move || make_audio_receiver(config_clone), config).await;
//     }
// }

pub struct RealtimeAudioSource {
    chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
    sample_buffer: VecDeque<f32>,
    sample_rate: u32,
    channels: u16,
}

impl RealtimeAudioSource {
    pub fn new(
        chunk_buffer: Arc<Mutex<RealTimeBuffer<AudioChunk>>>,
        sample_rate: u32,
        channels: u16,
    ) -> RealtimeAudioSource {
        RealtimeAudioSource {
            chunk_buffer,
            sample_buffer: VecDeque::new(),
            sample_rate,
            channels,
        }
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
            let mut buffer = self.chunk_buffer.lock().unwrap();
            if let Some(chunk) = buffer.next_item() {
                self.sample_buffer.extend(chunk.audio_data);
            }
        }
        self.sample_buffer.pop_front()
    }
}
