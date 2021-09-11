use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use cpal::traits::DeviceTrait;
use cpal::{Device, Sample, SampleFormat, Stream};

use quinn::ClientConfigBuilder;
use quinn::ConnectionError;
use quinn::Endpoint;
use quinn::NewConnection;

use crate::clerver::start_clerver;
use crate::processor::AudioProcessor;
use crate::protocol::PeerIdentity;
use crate::protocol::ProtocolMessage;
use crate::server::make_audio_receiver;
use crate::tui::Peer;
use crate::tui::PeerStatus;
use crate::tui::TuiEvent;
use crate::tui::TuiMessage;
use crate::InsanityConfig;

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
        if item.channels() == 1 {
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

async fn run_client(peer_socket_addr: SocketAddr) -> Result<NewConnection, ConnectionError> {
    struct SkipServerVerification;

    impl SkipServerVerification {
        fn new() -> Arc<Self> {
            Arc::new(Self)
        }
    }

    impl rustls::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _roots: &rustls::RootCertStore,
            _presented_certs: &[rustls::Certificate],
            _dns_name: webpki::DNSNameRef,
            _ocsp_response: &[u8],
        ) -> Result<rustls::ServerCertVerified, rustls::TLSError> {
            Ok(rustls::ServerCertVerified::assertion())
        }
    }

    let mut client_config = ClientConfigBuilder::default().build();
    let tls_config = Arc::get_mut(&mut client_config.crypto).unwrap();
    tls_config
        .dangerous()
        .set_certificate_verifier(SkipServerVerification::new());

    let mut endpoint_builder = Endpoint::builder();
    endpoint_builder.default_client_config(client_config);
    let (endpoint, _) = endpoint_builder
        .bind(&"0.0.0.0:0".parse().unwrap())
        .unwrap();

    endpoint
        .connect(&peer_socket_addr, "localhost")
        .unwrap()
        .await
}

#[tokio::main]
pub async fn start_client(own_address: String, peer_name: String, peer_address: String, config: InsanityConfig) {
    loop {
        if config
            .ui_message_sender
            .send(TuiEvent::Message(TuiMessage::UpdatePeer(
                peer_name.clone(),
                Peer {
                    name: peer_name.clone(),
                    status: PeerStatus::Disconnected,
                },
            )))
            .is_ok()
        {}

        let peer_socket_addr = *peer_address
            .to_socket_addrs()
            .expect("Invalid peer address")
            .collect::<Vec<SocketAddr>>()
            .get(0)
            .unwrap();

        match run_client(peer_socket_addr).await {
            Ok(conn) => {
                if config
                    .ui_message_sender
                    .send(TuiEvent::Message(TuiMessage::UpdatePeer(
                        peer_name.clone(),
                        Peer {
                            name: peer_name.clone(),
                            status: PeerStatus::Connected(peer_socket_addr),
                        },
                    )))
                    .is_ok()
                {}
                let config_clone = config.clone();

                let identification = ProtocolMessage::IdentityDeclaration(PeerIdentity::new(&own_address));
                match conn.connection.open_uni().await {
                    Ok(mut send) => { 
                        identification.write_to_stream(&mut send).await.unwrap();
                        send.finish().await.unwrap();
                    },
                    Err(_) => {},
                }

                start_clerver(conn, config.denoise, move || {
                    make_audio_receiver(config_clone)
                })
                .await;
                if config
                    .ui_message_sender
                    .send(TuiEvent::Message(TuiMessage::UpdatePeer(
                        peer_name.clone(),
                        Peer {
                            name: peer_name.clone(),
                            status: PeerStatus::Disconnected,
                        },
                    )))
                    .is_ok()
                {}
            }
            Err(e) => {
                // println!("{:?}", e);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}
