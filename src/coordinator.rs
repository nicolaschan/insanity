use std::{convert::Infallible, fs::File, io::Read, path::Path, sync::Arc, thread, time::Duration};

use serde::{Deserialize, Serialize};
use veq::veq::ConnectionInfo;
use warp::Filter;

use libtor::{HiddenServiceVersion, LogDestination, LogLevel, Tor, TorAddress, TorFlag};

use crate::protocol::{ConnectionManager, OnionAddress};

pub fn start_tor(config_dir: &Path, socks_port: u16, coordinator_port: u16) -> OnionAddress {
    let tor_data_dir = config_dir.join("tor-data");
    let tor_hs_dir = config_dir.join("tor-hs");
    let tor_hs_dir_clone = tor_hs_dir.clone();
    let tor_log_path = config_dir.join("tor.log");
    let _tor_handle = thread::spawn(move || {
        Tor::new()
            .flag(TorFlag::DataDirectory(
                tor_data_dir.to_string_lossy().to_string(),
            ))
            .flag(TorFlag::SocksPort(socks_port))
            .flag(TorFlag::HiddenServiceDir(
                tor_hs_dir_clone.to_string_lossy().to_string(),
            ))
            .flag(TorFlag::HiddenServiceVersion(HiddenServiceVersion::V3))
            .flag(TorFlag::HiddenServicePort(
                TorAddress::Port(coordinator_port),
                None.into(),
            ))
            .flag(TorFlag::LogTo(
                LogLevel::Notice,
                LogDestination::File(tor_log_path.to_string_lossy().to_string()),
            ))
            .flag(TorFlag::Quiet())
            // .flag(TorFlag::Log(LogLevel::Debug))
            .start()
            .unwrap();
    });

    loop {
        match File::open(tor_hs_dir.join("hostname")) {
            Ok(mut tor_hostname_file) => {
                let mut hostname_contents = String::new();
                tor_hostname_file
                    .read_to_string(&mut hostname_contents)
                    .unwrap();
                log::info!("Tor hostname: {}", hostname_contents);
                return OnionAddress::new(format!(
                    "{}:{}",
                    hostname_contents.trim(),
                    coordinator_port
                ))
                .unwrap();
            }
            Err(_) => {
                println!("Waiting for tor to start...");
                thread::sleep(Duration::from_millis(1000));
            }
        }
    }
}

fn with_c(
    connection_manager: Arc<ConnectionManager>,
) -> impl Filter<Extract = (Arc<ConnectionManager>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || connection_manager.clone())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AugmentedInfo {
    pub conn_info: ConnectionInfo,
    pub display_name: String,
}

pub async fn start_coordinator(coordinator_port: u16, connection_manager: Arc<ConnectionManager>) {
    let hello = warp::path!("hello" / String).map(|name| format!("Hello, {}!", name));
    // let peers_post = warp::post()
    //     .and(warp::path("peers"))
    //     .and(warp::body::json())
    //     .map(|peer: String| {
    //         warp::reply::json(&peer)
    //     });
    let info = warp::path("info")
        .and(with_c(connection_manager.clone()))
        .and_then(|c: Arc<ConnectionManager>| async move {
            let display_name = format!("{}@{}", whoami::username(), whoami::hostname());
            Ok::<_, Infallible>(warp::reply::json(&AugmentedInfo {
                conn_info: c.conn_info.clone(),
                display_name,
            }))
        });
    let id = warp::post()
        .and(warp::path!("id" / OnionAddress))
        .and(with_c(connection_manager.clone()))
        .and_then(|peer: OnionAddress, c: Arc<ConnectionManager>| async move {
            match c.id_or_new(peer).await {
                Some(id) => Ok(warp::reply::json(&id)),
                None => Err(warp::reject::reject()),
            }
        });
    let routes = hello.or(info).or(id);
    warp::serve(routes)
        .run(([127, 0, 0, 1], coordinator_port))
        .await;
}
