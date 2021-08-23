use std::fs::File;

use std::marker::Send;
use std::net::TcpListener;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::{Duration, SystemTime};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat, Stream};
use wav::BitDepth::Sixteen;

use crate::processor::{AudioChunk, AudioFormat};

fn run_input<T: Sample>(config: cpal::StreamConfig, device: Device, sender: Sender<f32>) -> Stream {
    let err_fn = |err| eprintln!("an error occurred in the input audio stream: {}", err);
    device
        .build_input_stream(
            &config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for sample in data.iter() {
                    if let Ok(()) = sender.send(sample.to_f32()) {}
                }
            },
            err_fn,
        )
        .unwrap()
}

fn setup_input_stream(device: Device, sender: Sender<f32>) -> Stream {
    let supported_configs_range = device.supported_input_configs().unwrap();
    let supported_config = find_stereo_input(supported_configs_range)
        .unwrap()
        .with_sample_rate(cpal::SampleRate(48000));
    let sample_format = supported_config.sample_format();
    let config = supported_config.into();
    // println!("Input {:?}", config);

    match sample_format {
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
    }
}

fn find_stereo_input(
    range: cpal::SupportedInputConfigs,
) -> Option<cpal::SupportedStreamConfigRange> {
    let mut something = None;
    for item in range {
        if item.channels() > 1 {
            return Some(item);
        } else {
            something = Some(item);
        }
    }
    something
}

pub fn start_server_with_receiver<R: AudioReceiver + 'static>(
    bind_address: String,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    let listener = TcpListener::bind(&bind_address)
        .expect("Could not start TCP server (port already in use?)");
    println!("Started TCP server on {}", bind_address);

    for mut stream in listener.incoming().flatten() {
        let make_receiver_clone = make_receiver.clone();
        thread::spawn(move || {
            let mut receiver = make_receiver_clone();
            let input_receiver = receiver.receiver();
            loop {
                let data = input_receiver.iter().take(4800).collect();
                let format = AudioFormat::new(0, 0);
                let audio_chunk = AudioChunk::new(format, data);
                audio_chunk.write_to_stream(&mut stream);
            }
        });
    }
}

struct CpalStreamReceiver {
    #[allow(dead_code)]
    input_stream: Stream,
    input_receiver: Receiver<f32>,
}

pub trait AudioReceiver {
    fn receiver(&mut self) -> &mut Receiver<f32>;
}

impl AudioReceiver for CpalStreamReceiver {
    fn receiver(&mut self) -> &mut Receiver<f32> {
        &mut self.input_receiver
    }
}

impl AudioReceiver for Receiver<f32> {
    fn receiver(&mut self) -> &mut Receiver<f32> {
        self
    }
}

fn make_audio_receiver() -> CpalStreamReceiver {
    let host = cpal::default_host();
    let (input_sender, input_receiver) = channel();
    let input_device = host
        .default_input_device()
        .expect("No default input device");
    let input_stream = setup_input_stream(input_device, input_sender);
    input_stream.play().unwrap();
    // If input_stream is dropped, then the input_receiver stops receiving data.
    // CpalStreamReceiver keeps input_stream alive along with input_receiver.
    CpalStreamReceiver {
        input_receiver,
        input_stream,
    }
}

fn make_music_receiver(path: String) -> Receiver<f32> {
    let (input_sender, input_receiver) = channel();
    thread::spawn(move || {
        let mut file = File::open(path).expect("Could not open sound file");
        let (header, data) = wav::read(&mut file).expect("Could not read sound (wav file)");
        // println!("Music: {:?}", header);
        if let Sixteen(vec) = data {
            let mut now = SystemTime::now();
            for chunk in vec.chunks(4800) {
                for val in chunk {
                    let s: i16 = Sample::from(val);
                    if input_sender.send(s.to_f32()).is_ok() {}
                }
                while now.elapsed().unwrap() < Duration::from_millis(50) {
                    std::hint::spin_loop();
                }
                now = SystemTime::now()
            }
        }
    });
    input_receiver
}

pub fn start_server(bind_address: String, music_path: Option<String>) {
    thread::spawn(move || {
        if let Some(path) = music_path {
            start_server_with_receiver(bind_address, move || make_music_receiver(path));
        } else {
            start_server_with_receiver(bind_address, make_audio_receiver);
        }
    });
}
