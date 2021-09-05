use std::convert::TryInto;
use std::error::Error;
use std::fs::File;

use std::marker::Send;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat, Stream};
use crossbeam::channel::{Receiver, Sender, unbounded};
use futures_util::StreamExt;
use quinn::{Certificate, CertificateChain, Endpoint, Incoming, PrivateKey, ServerConfig, ServerConfigBuilder, TransportConfig};
use wav::BitDepth::Sixteen;

use crate::clerver::start_clerver;
use crate::processor::AUDIO_CHUNK_SIZE;
use crate::tui::{Peer, PeerStatus, TuiEvent, TuiMessage};

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
        if item.channels() == 2 {
            return Some(item);
        } else {
            something = Some(item);
        }
    }
    something
}

async fn make_quic_server(bind_address: String) -> Result<Incoming, Box<dyn Error>> {
    let bind_socket_addr= *bind_address
        .to_socket_addrs()
        .expect("Invalid peer address")
        .collect::<Vec<SocketAddr>>()
        .get(0)
        .unwrap();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = cert.serialize_der().unwrap();
    let priv_key = cert.serialize_private_key_der();
    let priv_key = PrivateKey::from_der(&priv_key)?;

    let mut transport_config = TransportConfig::default();
    transport_config.max_concurrent_uni_streams(0).unwrap();
    let mut server_config = ServerConfig::default();
    server_config.transport = Arc::new(transport_config);
    let mut cfg_builder = ServerConfigBuilder::new(server_config);
    let cert = Certificate::from_der(&cert_der)?;
    cfg_builder.certificate(CertificateChain::from_certs(vec![cert]), priv_key)?;
    let server_config = cfg_builder.build();
 
    let mut endpoint_builder = Endpoint::builder();
    endpoint_builder.listen(server_config);
    let (_endpoint, incoming) = endpoint_builder.bind(&bind_socket_addr).unwrap();
    Ok(incoming)
}

pub async fn start_server_with_receiver<R: AudioReceiver + Send + 'static>(
    bind_address: String,
    denoise: bool,
    ui_message_sender: crossbeam::channel::Sender<TuiEvent>,
    make_receiver: impl (FnOnce() -> R) + Send + Clone + 'static,
) {
    println!("server: binding to {}", bind_address);
    let mut incoming = make_quic_server(bind_address).await.unwrap();
    loop {
        let incoming_conn = incoming.next().await.expect("1");
        println!("server: incoming conn");
        let mut conn = incoming_conn.await.expect("2");
        let make_receiver_clone = make_receiver.clone();
        start_clerver(conn, denoise, make_receiver_clone).await;
        println!("server: ending conn");
    }
}

pub struct CpalStreamReceiver {
    #[allow(dead_code)]
    input_stream: Mutex<Stream>,
    input_receiver: Receiver<f32>,
}

unsafe impl Send for CpalStreamReceiver {}

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

pub fn make_audio_receiver() -> CpalStreamReceiver {
    let host = cpal::default_host();
    let (input_sender, input_receiver) = unbounded();
    let input_device = host
        .default_input_device()
        .expect("No default input device");
    let input_stream = setup_input_stream(input_device, input_sender);
    input_stream.play().unwrap();
    // If input_stream is dropped, then the input_receiver stops receiving data.
    // CpalStreamReceiver keeps input_stream alive along with input_receiver.
    CpalStreamReceiver {
        input_receiver,
        input_stream: Mutex::new(input_stream),
    }
}

fn make_music_receiver(path: String) -> Receiver<f32> {
    let (input_sender, input_receiver) = unbounded();
    thread::spawn(move || {
        let mut file = File::open(path).expect("Could not open sound file");
        let (_, data) = wav::read(&mut file).expect("Could not read sound (wav file)");
        // println!("Music: {:?}", header);
        if let Sixteen(vec) = data {
            let mut now = SystemTime::now();
            for chunk in vec.chunks_exact(AUDIO_CHUNK_SIZE * 2) {
                for val in chunk {
                    let s: i16 = Sample::from(val);
                    if input_sender.send(s.to_f32()).is_ok() {}
                }
                while now.elapsed().unwrap() < Duration::from_millis(((AUDIO_CHUNK_SIZE * 1000) / 48000).try_into().unwrap()) {
                    std::hint::spin_loop();
                }
                now = SystemTime::now()
            }
        }
    });
    input_receiver
}

#[tokio::main]
pub async fn start_server(bind_address: String, denoise: bool, music_path: Option<String>, ui_message_sender: crossbeam::channel::Sender<TuiEvent>) {
    if let Some(path) = music_path {
        start_server_with_receiver(bind_address, denoise, ui_message_sender, move || make_music_receiver(path)).await;
    } else {
        start_server_with_receiver(bind_address, denoise, ui_message_sender, make_audio_receiver).await;
    }
}
