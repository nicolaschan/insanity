use std::net::SocketAddr;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use cpal::traits::DeviceTrait;
use cpal::{Device, Sample, SampleFormat, Stream};
use crossbeam::channel::Sender;
use ring::rand::SecureRandom;
use ring::rand::SystemRandom;

use crate::clerver::start_clerver;
use crate::processor::AudioProcessor;
use crate::server::make_audio_receiver;
use crate::tui::Peer;
use crate::tui::PeerStatus;
use crate::tui::TuiEvent;
use crate::tui::TuiMessage;

fn run_output<T: Sample>(
    config: cpal::StreamConfig,
    device: Device,
    processor: Arc<AudioProcessor<'static>>,
) -> Stream {
    let err_fn = |err| eprintln!("an error occurred in the output audio stream: {}", err);
    device
        .build_output_stream(
            &config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                processor.fill_buffer(data);
            },
            err_fn,
        )
        .unwrap()
}

fn find_stereo(range: cpal::SupportedOutputConfigs) -> Option<cpal::SupportedStreamConfigRange> {
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

pub fn setup_output_stream(device: Device, processor: Arc<AudioProcessor<'static>>) -> Stream {
    let supported_configs_range = device.supported_output_configs().unwrap();
    let supported_config = find_stereo(supported_configs_range)
        .unwrap()
        .with_sample_rate(cpal::SampleRate(48000));
    let sample_format = supported_config.sample_format();
    let config = supported_config.into();
    // println!("Output {:?}", config);

    match sample_format {
        SampleFormat::F32 => run_output::<f32>(config, device, processor),
        SampleFormat::I16 => run_output::<i16>(config, device, processor),
        SampleFormat::U16 => run_output::<u16>(config, device, processor),
    }
}

pub fn start_client(
    peer_address: String,
    _output_device_index: Option<usize>,
    enable_denoise: bool,
    ui_message_sender: Sender<TuiEvent>,
) {
    thread::spawn(move || -> ! {loop {
        if ui_message_sender.send(TuiEvent::Message(TuiMessage::UpdatePeer(peer_address.clone(), Peer {
            ip_address: peer_address.clone(),
            status: PeerStatus::Disconnected,
        }))).is_ok() {}

        let peer_socket_addr = *peer_address
            .to_socket_addrs()
            .expect("Invalid peer address")
            .collect::<Vec<SocketAddr>>()
            .get(0)
            .unwrap();

        // let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
        // config.set_max_idle_timeout(1000);
        // let mut scid = [0; quiche::MAX_CONN_ID_LEN];
        // SystemRandom::new().fill(&mut scid[..]).unwrap();
        // let scid = quiche::ConnectionId::from_ref(&scid);

        // match quiche::connect(None, &scid, peer_socket_addr, &mut config) {
        //     Ok(conn) => {
        //         if ui_message_sender.send(TuiEvent::Message(TuiMessage::UpdatePeer(peer_address.clone(), Peer {
        //             ip_address: peer_address.clone(),
        //             status: PeerStatus::Connected,
        //         }))).is_ok() {}

        //         start_clerver(conn, enable_denoise, make_audio_receiver);

        //         if ui_message_sender.send(TuiEvent::Message(TuiMessage::UpdatePeer(peer_address.clone(), Peer {
        //             ip_address: peer_address.clone(),
        //             status: PeerStatus::Disconnected,
        //         }))).is_ok() {}

        //     },
        //     Err(_) => {
        //         std::thread::sleep(std::time::Duration::from_millis(1000));
        //     },
        // };

        match TcpStream::connect_timeout(
            &peer_socket_addr,
            Duration::from_millis(1000),
        ) {
            Ok(stream) => {
                if ui_message_sender.send(TuiEvent::Message(TuiMessage::UpdatePeer(peer_address.clone(), Peer {
                    ip_address: peer_address.clone(),
                    status: PeerStatus::Connected,
                }))).is_ok() {}

                start_clerver(stream, enable_denoise, make_audio_receiver);

                if ui_message_sender.send(TuiEvent::Message(TuiMessage::UpdatePeer(peer_address.clone(), Peer {
                    ip_address: peer_address.clone(),
                    status: PeerStatus::Disconnected,
                }))).is_ok() {}
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
        }
    }});
}
