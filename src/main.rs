use std::fs::File;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use clap::{AppSettings, Clap};
use cpal::{Device, Sample, SampleFormat, Stream};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use wav::BitDepth::{Eight, Empty, Sixteen, ThirtyTwoFloat, TwentyFour};

use insanity::processor::{AudioChunk, AudioFormat};

#[derive(Clap)]
#[clap(version = "0.1.0", author = "Nicolas Chan <nicolas@nicolaschan.com>")]
#[clap(setting = AppSettings::ColoredHelp)]
struct Opts {
    #[clap(short, long)]
    list: bool,

    #[clap(long)]
    music: Option<String>,

    #[clap(short, long, default_value = "0.0.0.0:1337")]
    bind_address: String,

    #[clap(short, long, default_value = "127.0.0.1:1338")]
    peer_address: String,

    #[clap(short, long)]
    output_device: Option<usize>,
}

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
    println!("Input {:?}", config);

    match sample_format {
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
    }
}

fn run_output<T: Sample>(
    config: cpal::StreamConfig,
    device: Device,
    receiver: Receiver<f32>,
) -> Stream {
    let err_fn = |err| eprintln!("an error occurred in the output audio stream: {}", err);
    device
        .build_output_stream(
            &config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                for sample in data.iter_mut() {
                    if let Ok(val) = receiver.recv() {
                        *sample = Sample::from(&val);
                    }
                }
                for _ in 0..100 {
                    let _ = receiver.try_recv();
                }
            },
            err_fn,
        )
        .unwrap()
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

fn setup_output_stream(device: Device, receiver: Receiver<f32>) -> Stream {
    let supported_configs_range = device.supported_output_configs().unwrap();
    let supported_config = find_stereo(supported_configs_range)
        .unwrap()
        .with_sample_rate(cpal::SampleRate(48000));
    let sample_format = supported_config.sample_format();
    let config = supported_config.into();
    println!("Output {:?}", config);

    match sample_format {
        SampleFormat::F32 => run_output::<f32>(config, device, receiver),
        SampleFormat::I16 => run_output::<i16>(config, device, receiver),
        SampleFormat::U16 => run_output::<u16>(config, device, receiver),
    }
}

fn main() {
    let opts: Opts = Opts::parse();

    if opts.list {
        let host = cpal::default_host();
        let input_devices = host.input_devices().expect("Could not get input devices");
        println!("Input devices");
        for (i, dev) in input_devices.enumerate() {
            println!("  {}: {:?}", i, dev.name());
        }

        println!("\nOutput devices");
        let output_devices = host.output_devices().expect("Could not get output devices");
        for (i, dev) in output_devices.enumerate() {
            println!("  {}: {:?}", i, dev.name());
        }

        println!("\nDefaults");
        println!(
            "  input: {:?}",
            host.default_input_device()
                .expect("No default input device")
                .name()
        );
        dbg!(host.default_input_device().unwrap().name().unwrap());
        println!(
            "  output: {:?}",
            host.default_output_device()
                .expect("No default output device")
                .name()
        );
    } else {
        let listener = TcpListener::bind(&opts.bind_address)
            .expect("Could not start TCP server (port already in use?)");
        println!("Started TCP server on {}", &opts.bind_address);
        let path = opts.music.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                let music_path = path.clone();
                if let Ok(mut stream) = stream {
                    thread::spawn(move || {
                        let host = cpal::default_host();
                        let (input_sender, input_receiver) = channel();
                        println!("Peer connected from {:?}", stream.peer_addr());
                        match music_path.clone() {
                            Some(path) => {
                                let mut file = File::open(path).expect("Could not open file");
                                let (header, data) =
                                    wav::read(&mut file).expect("Could not read sound (wav file?)");
                                println!("Music: {:?}", header);
                                match data {
                                    Eight(vec) => {
                                        for val in vec.iter() {
                                            if stream.write(&val.to_le_bytes()).is_ok() {}
                                        }
                                    }
                                    Sixteen(vec) => {
                                        for chunk in vec.chunks(4800) {
                                            let format = AudioFormat::new(
                                                header.channel_count,
                                                header.sampling_rate,
                                            );
                                            let mut data = Vec::new();
                                            for subchunk in chunk.chunks(2) {
                                                let left: i16 =
                                                    Sample::from(subchunk.get(0).unwrap());
                                                let right: i16 =
                                                    Sample::from(subchunk.get(1).unwrap());

                                                data.push(left.to_f32());
                                                data.push(right.to_f32());
                                            }
                                            let audio_chunk = AudioChunk::new(format, data);
                                            audio_chunk.write_to_stream(&stream);
                                        }
                                    }
                                    TwentyFour(vec) => {
                                        for val in vec.iter() {
                                            if stream.write(&val.to_le_bytes()).is_ok() {}
                                        }
                                    }
                                    ThirtyTwoFloat(vec) => {
                                        for val in vec.iter() {
                                            if stream.write(&val.to_le_bytes()).is_ok() {}
                                        }
                                    }
                                    Empty => {}
                                }
                            }
                            None => {
                                let input_device = host
                                    .default_input_device()
                                    .expect("No default input device");
                                let input_stream = setup_input_stream(input_device, input_sender);
                                input_stream.play().unwrap();

                                loop {
                                    let data = input_receiver.iter().take(4800).collect();
                                    let format = AudioFormat::new(0, 0);
                                    let audio_chunk = AudioChunk::new(format, data);
                                    audio_chunk.write_to_stream(&stream);
                                }
                            }
                        }
                    });
                }
            }
        });

        let output_device_index = opts.output_device;
        loop {
            println!("Attempting to connect to {}", &opts.peer_address);
            match TcpStream::connect(&opts.peer_address) {
                Ok(stream) => {
                    let host = cpal::default_host();
                    let (output_sender, output_receiver) = channel();
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
                    let output_stream = setup_output_stream(output_device, output_receiver);
                    output_stream.play().unwrap();

                    loop {
                        if let Ok(audio_chunk) = AudioChunk::read_from_stream(&stream) {
                            for val in audio_chunk.audio_data.iter() {
                                if let Ok(()) = output_sender.send(*val) {}
                            }
                        } else {
                            break;
                        }
                    }
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            }
        }
    }
}
