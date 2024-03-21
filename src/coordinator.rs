use std::{convert::Infallible, net::{IpAddr, Ipv4Addr, SocketAddr}, path::Path, sync::Arc, time::Duration};
use std::io::Write;

use serde::{Deserialize, Serialize};
use tor_config::CfgPath;
use tor_hsrproxy::{config::{Encapsulation, ProxyAction, ProxyPattern, ProxyRule, TargetAddr, ProxyConfigBuilder}, OnionServiceReverseProxy};
use tor_hsservice::{config::OnionServiceConfigBuilder, HsNickname};
use veq::veq::ConnectionInfo;
use warp::Filter;

use crate::protocol::{ConnectionManager, OnionAddress};


use arti_client::{TorClient, TorClientConfig};

pub async fn start_tor(config_dir: &Path, coordinator_port: u16) -> (TorClient<tor_rtcompat::PreferredRuntime>, OnionAddress) {
    let tor_cache_dir = config_dir.join("tor-cache");
    let tor_state_dir = config_dir.join("tor-state");

    let mut client_config_builder = TorClientConfig::builder();
    client_config_builder.storage().cache_dir(CfgPath::new_literal(tor_cache_dir)).state_dir(CfgPath::new_literal(tor_state_dir));
    client_config_builder.stream_timeouts().connect_timeout(Duration::from_secs(10));
    let client_config = client_config_builder.build().expect("Failed to set up tor client config.");

    let client_handle = TorClient::create_bootstrapped(client_config);
    log::info!("Bootstrapping Tor client...");
    // Including print to stdout since TUI doesn't appear until Tor is started.
    print!("Bootstrapping Tor client...");
    std::io::stdout().flush().expect("Failed to flush stdout.");
    let mut client = client_handle.await.expect("Failed to bootstrap tor client.");
    log::info!("Bootstrapping Tor client done.");
    println!("done.");

    // Enable Tor client to connect to onion services.
    let stream_prefs = {
        let mut stream_prefs = arti_client::StreamPrefs::default();
        stream_prefs.connect_to_onion_services(arti_client::config::BoolOrAuto::Explicit(true));
        stream_prefs
    };
    client.set_stream_prefs(stream_prefs);

    // Launch onion service.
    let nickname = HsNickname::new(coordinator_port.to_string()).expect("Failed to create tor onion nickname.");
    let onion_service_config = OnionServiceConfigBuilder::default().nickname(nickname.clone()).build().expect("Failed to build tor onion service config.");
    let (onion_service, request_stream) = client.launch_onion_service(onion_service_config).expect("Failed to launch tor onion service");
    let onion_name: String = onion_service.onion_name().expect("Failed to extract onion service name").to_string().trim().to_string();

    // Start task for forwarding connections to the onion service to a local port.
    {
        let onion_service = onion_service.clone();
        let client = client.clone();
        tokio::spawn(async move {
            log::info!("Onion service started.");
            // Needed to prevent onion service from being dropped.
            // The onion service dropping ends the reverse proxy.
            let _onion_service = onion_service;
            // Forward onion:coordinator_port to localhost:coordinator_port.
            let proxy_rule = ProxyRule::new(
                ProxyPattern::one_port(coordinator_port).expect("Failed to set up tor proxy pattern."),
                ProxyAction::Forward(Encapsulation::Simple, TargetAddr::Inet(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), coordinator_port)))
            );
            let mut proxy_config_builder = ProxyConfigBuilder::default();
            proxy_config_builder.proxy_ports().push(proxy_rule);
            let proxy_config = proxy_config_builder.build().expect("Failed to set up tor proxy config.");

            // Start reverse proxy.
            // This should remain on for the entire duration insanity is running.
            let reverse_proxy = OnionServiceReverseProxy::new(proxy_config);
            let res = reverse_proxy.handle_requests(client.runtime().clone(), nickname, request_stream).await;

            match res {
                Ok(()) => log::error!("Onion service ended cleanly."),
                Err(e) => log::error!("Onion service ended with error {e}."),
            }
            panic!("Onion service ended early.");
        });
    }

    let onion_address = OnionAddress::new(format!("{}:{}", onion_name, coordinator_port));
    (client, onion_address)
}

fn with_c(
    connection_manager: Arc<ConnectionManager>,
) -> impl Filter<Extract = (Arc<ConnectionManager>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || connection_manager.clone())
}

fn with_display_name(
    display_name: String,
) -> impl Filter<Extract = (String,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || display_name.clone())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AugmentedInfo {
    pub conn_info: ConnectionInfo,
    pub display_name: String,
}

pub async fn start_coordinator(
    coordinator_port: u16,
    connection_manager: Arc<ConnectionManager>,
    display_name: String,
) {
    let hello = warp::path!("hello" / String).map(|name| format!("Hello, {name}!"));
    // let peers_post = warp::post()
    //     .and(warp::path("peers"))
    //     .and(warp::body::json())
    //     .map(|peer: String| {
    //         warp::reply::json(&peer)
    //     });
    let info = warp::path("info")
        .and(with_c(connection_manager.clone()))
        .and(with_display_name(display_name.clone()))
        .and_then(
            |c: Arc<ConnectionManager>, display_name: String| async move {
                Ok::<_, Infallible>(warp::reply::json(&AugmentedInfo {
                    conn_info: c.conn_info.clone(),
                    display_name,
                }))
            },
        );
    let id = warp::post()
        .and(warp::path!("id" / OnionAddress))
        .and(with_c(connection_manager.clone()))
        .and_then(|peer: OnionAddress, c: Arc<ConnectionManager>| async move {
            match c.id_or_new(&peer).await {
                Some(id) => Ok(warp::reply::json(&id)),
                None => Err(warp::reject::reject()),
            }
        });
    let routes = hello.or(info).or(id);
    warp::serve(routes)
        .run(([127, 0, 0, 1], coordinator_port))
        .await;
}
