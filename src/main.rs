use clap::{AppSettings, Clap};
use cpal::{Sample, SampleFormat, Stream, Device};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};

#[derive(Clap)]
#[clap(version = "0.1.0", author = "Nicolas Chan <nicolas@nicolaschan.com>")]
#[clap(setting = AppSettings::ColoredHelp)]
struct Opts {
    #[clap(short, long)]
    list: bool,

    #[clap(short, long, default_value = "0.0.0.0:1337")]
    bind_address: String,

    #[clap(short, long, default_value = "127.0.0.1:1338")]
    peer_address: String
}

fn run_input<T: Sample>(config: cpal::StreamConfig, device: Device, sender: Sender<f32>) -> Stream {
    let err_fn = |err| eprintln!("an error occurred in the input audio stream: {}", err);
    device.build_input_stream(&config, move |data: &[T], _: &cpal::InputCallbackInfo| {
        for sample in data.iter() {
            if let Ok(()) = sender.send(sample.to_f32()) {}
        }
    }, err_fn).unwrap()
}

fn setup_input_stream(device: Device, sender: Sender<f32>) -> Stream {
    let mut supported_configs_range = device.supported_input_configs().unwrap();
    let supported_config = supported_configs_range.next().unwrap().with_max_sample_rate();
    let sample_format = supported_config.sample_format();
    let config = supported_config.into();
    println!("{:?}", config);
    
    match sample_format {
        SampleFormat::F32 => run_input::<f32>(config, device, sender),
        SampleFormat::I16 => run_input::<i16>(config, device, sender),
        SampleFormat::U16 => run_input::<u16>(config, device, sender),
    }
}

fn run_output<T: Sample>(config: cpal::StreamConfig, device: Device, receiver: Receiver<f32>) -> Stream {
    let err_fn = |err| eprintln!("an error occurred in the output audio stream: {}", err);
    device.build_output_stream(&config, move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        // let vec = receiver.recv().unwrap();
        for sample in data.iter_mut() {
            // *sample = Sample::from(&val);
            // *sample = Sample::from(&0.0);
            // *sample = Sample::from(&rand::random::<f32>());
            *sample = Sample::from(&receiver.recv().unwrap());
        }
    }, err_fn).unwrap()
}

fn setup_output_stream(device: Device, receiver: Receiver<f32>) -> Stream {
    let mut supported_configs_range = device.supported_output_configs().unwrap();
    let supported_config = supported_configs_range.next().unwrap().with_max_sample_rate();
    let sample_format = supported_config.sample_format();
    let config = supported_config.into();
    println!("{:?}", config);
    
    match sample_format {
        SampleFormat::F32 => run_output::<f32>(config, device, receiver),
        SampleFormat::I16 => run_output::<i16>(config, device, receiver),
        SampleFormat::U16 => run_output::<u16>(config, device, receiver),
    }
}

fn main() {
    let opts: Opts = Opts::parse();
    let host = cpal::default_host();

    if opts.list {
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
        println!("  input: {:?}", host.default_input_device().expect("No default input device").name());
        println!("  output: {:?}", host.default_output_device().expect("No default output device").name());
    } else {
        let input_device = host.default_input_device().expect("No default input device");
        let output_device = host.default_output_device().expect("No default output device");

        let (input_sender, input_receiver) = channel();
        let (output_sender, output_receiver) = channel();

        let listener = TcpListener::bind(opts.bind_address).expect("Could not start TCP server (port already in use?)");


        let output_stream = setup_output_stream(output_device, output_receiver);
        output_stream.play().unwrap();

        thread::spawn(move || {
            let mut stream = listener.incoming().next().unwrap().unwrap();
            let input_stream = setup_input_stream(input_device, input_sender);
            input_stream.play().unwrap();
            println!("Peer connected from {:?}", stream.peer_addr());
            for val in input_receiver.iter() {
                stream.write_all(&val.to_le_bytes()).unwrap();
            }
        });

        loop {
            match TcpStream::connect(&opts.peer_address) {
                Ok(mut stream) => loop {
                    let mut val = [0; 4];
                    stream.read_exact(&mut val).unwrap();
                    if let Ok(()) = output_sender.send(f32::from_le_bytes(val)) {}
                },
                Err(_) => { 
                    eprintln!("Could not connect to peer at {}", &opts.peer_address);
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                },
            }
        }
    }
}
