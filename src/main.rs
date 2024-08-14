use std::{collections::BTreeMap, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use clap::Parser;
use futures_util::stream::FuturesUnordered;
use insanity::{
    coordinator::{create_tor_client, forward_onion_connections}, // , start_coordinator}, // , start_tor},
    managed_peer::ManagedPeer,
    protocol::{ConnectionManager, OnionAddress},
};
use insanity_tui::{AppEvent, UserAction};
use std::iter::Iterator;

use futures_util::StreamExt;
use veq::{snow_types::SnowKeypair, veq::VeqSocket};

#[derive(Parser, Debug)]
#[clap(version = "0.1.0", author = "Nicolas Chan <nicolas@nicolaschan.com>")]
struct Opts {
    /// Enables denoise by default for all peers upon connection.
    #[clap(short, long)]
    denoise: bool,

    // #[clap(long)]
    // music: Option<String>,

    #[clap(short, long, default_value = "1337")]
    listen_port: u16,

    /// Address of peer to connect to.
    #[clap(short, long)]
    peer: Vec<String>,

    // #[clap(long)]
    // id: Option<String>,

    /// Disables the terminal user interface.
    #[clap(long)]
    no_tui: bool,

    // #[clap(long, default_value = "48000")]
    // sample_rate: usize,

    // #[clap(long, default_value = "2")]
    // channels: usize,

    /// Nickname to differentiate between onion services.
    #[clap(long, default_value = "default")]
    onion_nickname: String,

    /// Directory to store insanity data.
    #[clap(long)]
    dir: Option<String>,
}

#[tokio::main]
async fn main() {
    let opts: Opts = Opts::parse();

    let display_name = format!("{}@{}", whoami::username(), whoami::fallible::hostname().unwrap_or(String::from("unknown")));

    let insanity_dir = match opts.dir {
        Some(dir) => PathBuf::from_str(&dir).unwrap(),
        None => dirs::data_local_dir().expect("no data directory!?").join("insanity"),
    };
    std::fs::create_dir_all(&insanity_dir).expect("could not create insanity data directory");

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d %H:%M:%S]"),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Debug)
        .chain(
            fern::log_file(insanity_dir.join("insanity.log"))
                .expect("could not create insanity log file"),
        )
        .apply()
        .expect("could not setup logging");
    log::info!("Starting insanity");

    let tor_dir = insanity_dir.join("tor");
    std::fs::create_dir_all(&tor_dir).expect("could not create tor data directory");

    let onion_nickname = opts.onion_nickname;
    let (tor_client, onion_service, onion_request_stream) = create_tor_client(&tor_dir, onion_nickname).await;
    let onion_name = onion_service.onion_name().expect("Failed to extract onion service name").to_string();
    let onion_address = OnionAddress::new(format!("{}:{}", onion_name, insanity::coordinator::COORDINATOR_PORT));

    let sled_path = insanity_dir.join("data.sled");
    let db = sled::open(sled_path).unwrap();

    let keypair: SnowKeypair = match db
        .get("private_key")
        .unwrap()
        .and_then(|v| bincode::deserialize::<SnowKeypair>(&v).ok())
    {
        Some(keypair) => {
            log::debug!(
                "Found keypair in database. Public Key: {:?}",
                keypair.public()
            );
            keypair
        }
        None => {
            log::debug!("No keypair found in db, generating one");
            let keypair = SnowKeypair::new().expect("Failed to generate keypair");
            db.insert("private_key", bincode::serialize(&keypair).unwrap())
                .unwrap();
            keypair
        }
    };

    let socket = VeqSocket::bind_with_keypair(format!("0.0.0.0:{}", opts.listen_port), keypair).await.unwrap();
    log::debug!("Connection info: {:?}", socket.connection_info());
    let connection_manager = ConnectionManager::new(socket.connection_info(), tor_client, onion_address.clone(), db);
    println!("Own address: {onion_address:?}");
    let connection_manager_arc = Arc::new(connection_manager);

    let connection_manager_arc_clone = connection_manager_arc.clone();
    let name_copy = display_name.clone();
    tokio::spawn(async move {
        forward_onion_connections(onion_request_stream, connection_manager_arc_clone, name_copy).await;
    });

    let (app_event_sender, user_action_receiver, handle) = if !opts.no_tui {
        let (x, y, z) = insanity_tui::start_tui().await.unwrap();
        x.send(AppEvent::SetOwnAddress(onion_address.to_string())).unwrap();
        x.send(AppEvent::SetOwnDisplayName(display_name)).unwrap();
        (Some(x), Some(y), Some(z))
    } else {
        (None, None, None)
    };

    let peer_list = opts.peer;
    let denoise = opts.denoise;

    let managed_peers = Arc::new(
        peer_list
            .into_iter()
            .map(|addr| OnionAddress::new(addr))
            .zip(std::iter::repeat((
                socket.clone(),
                connection_manager_arc,
                app_event_sender.clone(),
            )))
            .map(
                |(peer, (socket, conn_manager, app_event_sender))| async move {
                    (
                        peer.clone().to_string(),
                        ManagedPeer::new(peer, denoise, 100, socket, conn_manager, app_event_sender).await)
                },
            )
            .collect::<FuturesUnordered<_>>()
            .collect::<BTreeMap<_, _>>()
            .await,
    );

    let managed_peers_clone = managed_peers.clone();
    if let Some(mut receiver) = user_action_receiver {
        tokio::spawn(async move {
            while let Some(action) = receiver.recv().await {
                match action {
                    UserAction::DisableDenoise(id) => {
                        if let Some(peer) = managed_peers_clone.get(&id) {
                            peer.set_denoise(false).await;
                        }
                    }
                    UserAction::EnableDenoise(id) => {
                        if let Some(peer) = managed_peers_clone.get(&id) {
                            peer.set_denoise(true).await;
                        }
                    }
                    UserAction::DisablePeer(id) => {
                        if let Some(peer) = managed_peers_clone.get(&id) {
                            peer.disable().await;
                        }
                    }
                    UserAction::EnablePeer(id) => {
                        if let Some(peer) = managed_peers_clone.get(&id) {
                            peer.enable().await;
                        }
                    }
                    UserAction::SetVolume(id, volume) => {
                        if let Some(peer) = managed_peers_clone.get(&id) {
                            peer.set_volume(volume).await;
                        }
                    }
                    UserAction::SendMessage(message) => {
                        for (_, peer) in managed_peers_clone.iter() {
                            peer.send_message(message.clone());
                        }
                    }
                }
            }
        });
    }

    for peer in managed_peers.values() {
        peer.enable().await;
    }

    if let Some(handle) = handle {
        insanity_tui::stop_tui(handle).await.unwrap();
    } else {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
