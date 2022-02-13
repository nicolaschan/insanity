use std::sync::Arc;

use cpal::traits::DeviceTrait;
use cpal::{Device, Sample, SampleFormat, Stream, StreamConfig, SampleRate, BufferSize};

use crate::processor::AudioProcessor;
use crate::processor::AUDIO_CHANNELS;

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
        if item.channels() == AUDIO_CHANNELS {
            return Some(item);
        } else {
            something = Some(item);
        }
    }
    something
}

pub fn setup_output_stream(sample_format: SampleFormat, config: StreamConfig, device: Device, processor: Arc<AudioProcessor<'static>>) -> Stream {
    match sample_format {
        SampleFormat::F32 => run_output::<f32>(config, device, processor),
        SampleFormat::I16 => run_output::<i16>(config, device, processor),
        SampleFormat::U16 => run_output::<u16>(config, device, processor),
    }
}

pub fn get_output_config(device: &Device) -> (SampleFormat, StreamConfig) {
    let supported_configs_range = device.supported_output_configs().unwrap();
    let supported_config_range = find_stereo(supported_configs_range)
        .unwrap();
    let max_sample_rate = supported_config_range.max_sample_rate();

    let channels = supported_config_range.channels();
    let sample_rate = std::cmp::min(SampleRate(48000), max_sample_rate);
    let buffer_size = match supported_config_range.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => BufferSize::Default,
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    };
    println!("buffer size: {:?}", buffer_size);
    let supported_config = StreamConfig { channels, sample_rate, buffer_size };

    // let supported_config = supported_config_range.with_sample_rate(std::cmp::min(SampleRate(48000), max_sample_rate));
    let sample_format = supported_config_range.sample_format();
    let config = supported_config.into();
    (sample_format, config)
}

// async fn run_client(peer_socket_addr: SocketAddr) -> VeqSocket {
//     VeqSocket::bind(format!("0.0.0.0:{}", ))
// }

// #[tokio::main]
// pub async fn start_client(
//     own_address: String,
//     peer_name: String,
//     peer_address: String,
//     config: InsanityConfig,
// ) {
//     loop {
//         if config
//             .ui_message_sender
//             .send(TuiEvent::Message(TuiMessage::UpdatePeer(
//                 peer_name.clone(),
//                 Peer {
//                     name: peer_name.clone(),
//                     status: PeerStatus::Disconnected,
//                 },
//             )))
//             .is_ok()
//         {}

//         let peer_socket_addr = *peer_address
//             .to_socket_addrs()
//             .expect("Invalid peer address")
//             .collect::<Vec<SocketAddr>>()
//             .get(0)
//             .unwrap();

//         match run_client(peer_socket_addr).await {
//             Ok(conn) => {
//                 if config
//                     .ui_message_sender
//                     .send(TuiEvent::Message(TuiMessage::UpdatePeer(
//                         peer_name.clone(),
//                         Peer {
//                             name: peer_name.clone(),
//                             status: PeerStatus::Connected(peer_socket_addr),
//                         },
//                     )))
//                     .is_ok()
//                 {}
//                 let config_clone = config.clone();

//                 let identification =
//                     ProtocolMessage::IdentityDeclaration(PeerIdentity::new(&own_address));
//                 if let Ok(mut send) = conn.connection.open_uni().await {
//                     identification.write_to_stream(&mut send).await.unwrap();
//                     send.finish().await.unwrap();
//                 }

//                 start_clerver(conn, config.denoise, move || {
//                     make_audio_receiver(config_clone)
//                 })
//                 .await;
//                 if config
//                     .ui_message_sender
//                     .send(TuiEvent::Message(TuiMessage::UpdatePeer(
//                         peer_name.clone(),
//                         Peer {
//                             name: peer_name.clone(),
//                             status: PeerStatus::Disconnected,
//                         },
//                     )))
//                     .is_ok()
//                 {}
//             }
//             Err(_e) => {
//                 // println!("{:?}", e);
//             }
//         }
//         tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
//     }
// }
