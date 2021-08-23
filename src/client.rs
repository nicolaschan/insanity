use std::net::SocketAddr;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat, Stream};

use crate::processor::{AudioChunk, AudioProcessor};

fn run_output<T: Sample>(
    config: cpal::StreamConfig,
    device: Device,
    processor: Arc<Mutex<AudioProcessor<'static>>>,
) -> Stream {
    let err_fn = |err| eprintln!("an error occurred in the output audio stream: {}", err);
    device
        .build_output_stream(
            &config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                processor.lock().unwrap().fill_buffer(data);
            },
            err_fn,
        )
        .unwrap()
}

fn find_stereo(range: cpal::SupportedOutputConfigs) -> Option<cpal::SupportedStreamConfigRange> {
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

fn setup_output_stream(device: Device, procesor: Arc<Mutex<AudioProcessor<'static>>>) -> Stream {
    let supported_configs_range = device.supported_output_configs().unwrap();
    let supported_config = find_stereo(supported_configs_range)
        .unwrap()
        .with_sample_rate(cpal::SampleRate(48000));
    let sample_format = supported_config.sample_format();
    let config = supported_config.into();
    println!("Output {:?}", config);

    match sample_format {
        SampleFormat::F32 => run_output::<f32>(config, device, procesor),
        SampleFormat::I16 => run_output::<i16>(config, device, procesor),
        SampleFormat::U16 => run_output::<u16>(config, device, procesor),
    }
}

pub fn start_client(
    peer_address: String,
    output_device_index: Option<usize>,
    enable_denoise: bool,
) {
    thread::spawn(move || loop {
        println!("Attempting to connect to {}", peer_address);
        match TcpStream::connect_timeout(
            peer_address
                .to_socket_addrs()
                .expect("Invalid peer address")
                .collect::<Vec<SocketAddr>>()
                .get(0)
                .unwrap(),
            Duration::from_millis(1000),
        ) {
            Ok(mut stream) => {
                let host = cpal::default_host();
                let output_device = match output_device_index {
                    Some(i) => host
                        .output_devices()
                        .expect("No output devices")
                        .collect::<Vec<Device>>()
                        .swap_remove(i),
                    None => host
                        .default_output_device()
                        .expect("No default output device"),
                };

                let processor = Arc::new(Mutex::new(AudioProcessor::new(enable_denoise)));
                let output_stream = setup_output_stream(output_device, processor.clone());
                output_stream.play().unwrap();

                while let Ok(audio_chunk) = AudioChunk::read_from_stream(&mut stream) {
                    processor.lock().unwrap().handle_incoming(audio_chunk);
                }
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
        }
    });
}
