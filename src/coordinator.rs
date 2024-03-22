use std::{path::Path, sync::Arc, time::Duration};
use std::io::Write;

use http_body_util::Full;
use serde::{Deserialize, Serialize};
use tor_config::CfgPath;
use tor_hsservice::{config::OnionServiceConfigBuilder, HsNickname, RendRequest, StreamRequest, RunningOnionService};
use tor_proto::stream::IncomingStreamRequest;
use tor_cell::relaycell::msg::Connected;
use veq::veq::ConnectionInfo;

use crate::protocol::ConnectionManager;
use futures::Stream;
use futures::StreamExt;

use hyper_util::rt::TokioIo;
use hyper::{body::Bytes, server::conn::http1, Method, Response, StatusCode};
use hyper::service::service_fn;

use arti_client::{TorClient, TorClientConfig};

// All onion services listen on this port.
pub const COORDINATOR_PORT: u16 = 11337;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AugmentedInfo {
    pub conn_info: ConnectionInfo,
    pub display_name: String,
}

pub async fn create_tor_client(config_dir: &Path, nickname: String) -> (TorClient<tor_rtcompat::PreferredRuntime>, Arc<RunningOnionService>, impl Stream<Item = RendRequest>) {
    let tor_cache_dir = config_dir.join("tor-cache");
    let tor_state_dir = config_dir.join("tor-state");

    let mut client_config_builder = TorClientConfig::builder();
    client_config_builder.storage().cache_dir(CfgPath::new_literal(tor_cache_dir)).state_dir(CfgPath::new_literal(tor_state_dir));
    client_config_builder.stream_timeouts().connect_timeout(Duration::from_secs(10));
    let client_config = client_config_builder.build().expect("Failed to set up tor client config.");

    let client_handle = TorClient::create_bootstrapped(client_config.clone());
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

    let hs_nickname = HsNickname::new(nickname).expect("Failed to create tor onion nickname.");
    let onion_service_config = OnionServiceConfigBuilder::default().nickname(hs_nickname).build().expect("Failed to build tor onion service config.");
    let (onion_service, request_stream) = client.launch_onion_service(onion_service_config).expect("Failed to launch tor onion service");

    (client, onion_service, request_stream)
}


pub async fn forward_onion_connections(request_stream: impl Stream<Item = RendRequest> + std::marker::Unpin, connection_manager: Arc<ConnectionManager>, display_name: String) {
    log::info!("New onion service started.");
    let stream_requests = tor_hsservice::handle_rend_requests(request_stream);
    tokio::pin!(stream_requests);

    while let Some(stream_request) = stream_requests.next().await {
        let connection_manager = connection_manager.clone();
        let display_name = display_name.clone();
        tokio::spawn(async move {
            handle_stream_request(stream_request, connection_manager, display_name).await;
        });
    }
    log::error!("Onion service ended early.");
}

async fn handle_stream_request(
    stream_request: StreamRequest, connection_manager: Arc<ConnectionManager>, display_name: String
) {
    match stream_request.request() {
        &IncomingStreamRequest::Begin(ref begin) if begin.port() == COORDINATOR_PORT => {
            let onion_service_stream = stream_request.accept(Connected::new_empty()).await.unwrap();
            let io = TokioIo::new(onion_service_stream);
            http1::Builder::new().serve_connection(io, service_fn(|request| serve(request, connection_manager.clone(), display_name.clone()))).await.unwrap();
        }
        _ => {
            log::debug!("Received request to onion service on wrong port.");
            stream_request.shutdown_circuit().unwrap();
        }
    }
}

async fn serve(request: hyper::Request<hyper::body::Incoming>, connection_manager: Arc<ConnectionManager>, display_name: String) -> Result<hyper::Response<Full<Bytes>>, http::Error> {
    let path = request.uri().path();
    log::debug!("Path: {path}");

    // Assume the path begins with '/' and skip the first token which is the empty string.
    let mut parts = path.split('/').skip(1);
    let first = parts.next().ok_or(()).unwrap();
    log::debug!("Path first: {first}");

    match (request.method(), first) {
        (&Method::GET, "hello") => {
            let name = parts.next().ok_or(()).unwrap();
            Ok(hyper::Response::new(Full::new(Bytes::from(
                format!("Hello, {name}!")
            ))))
        },
        (&Method::GET, "info") => {
            log::debug!("Path info.");
            let info = AugmentedInfo {
                conn_info: connection_manager.conn_info.clone(),
                display_name,
            };
            let json = serde_json::to_string(&info).unwrap();
            Ok(hyper::Response::new(Full::new(Bytes::from(
                json
            ))))
        },
        _ => {
            // TODO: probably something better to respond with then empty bytes.
            log::error!("Unexpected path.");
            let mut not_found = Response::new(Full::new(Bytes::from("")));
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}