use std::{
    collections::{BTreeMap},
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc,
    },
    time::Duration,
};

use clap::Parser;
use futures_util::stream::FuturesUnordered;
use insanity::{
    coordinator::{start_coordinator, start_tor},
    managed_peer::ManagedPeer,
    protocol::{ConnectionManager, OnionAddress},
};
use insanity_tui::{AppEvent, UserAction};
use std::iter::Iterator;


use futures_util::StreamExt;
use veq::{
    snow_types::{SnowKeypair, SnowPrivateKey},
    veq::VeqSocket,
};

#[derive(Parser, Debug)]
#[clap(version = "0.1.0", author = "Nicolas Chan <nicolas@nicolaschan.com>")]
struct Opts {
    #[clap(short, long)]
    denoise: bool,

    #[clap(long)]
    music: Option<String>,

    #[clap(short, long, default_value = "1337")]
    listen_port: u16,

    #[clap(long)]
    peer: Vec<String>,

    #[clap(long)]
    id: Option<String>,

    #[clap(long)]
    no_tui: bool,

    #[clap(long, default_value = "48000")]
    sample_rate: usize,

    #[clap(long, default_value = "2")]
    channels: usize,

    #[clap(long, default_value = "19050")]
    socks_port: u16,

    #[clap(long, default_value = "11337")]
    coordinator_port: u16,

    #[clap(long)]
    dir: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opts: Opts = Opts::parse();

    let _id: String = match opts.id {
        Some(id) => id,
        None => nanoid::nanoid!(),
    };

    let insanity_dir = match opts.dir {
        Some(dir) => PathBuf::from_str(&dir).unwrap(),
        None => dirs::data_local_dir()
            .expect("no data directory!?")
            .join("insanity"),
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
    let socks_port = opts.socks_port;
    let coordinator_port = opts.coordinator_port;
    let onion_address = start_tor(&tor_dir, socks_port, coordinator_port);

    let proxy = reqwest::Proxy::all(format!("socks5h://127.0.0.1:{}", socks_port)).unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .proxy(proxy)
        .build()
        .unwrap();

    let sled_path = insanity_dir.join("data.sled");
    let db = sled::open(sled_path).unwrap();
    let private_key: SnowPrivateKey = match db.get("private_key").unwrap() {
        Some(key) => bincode::deserialize(&key).unwrap(),
        None => {
            let key = SnowKeypair::new().private();
            db.insert("private_key", bincode::serialize(&key).unwrap())
                .unwrap();
            key
        }
    };

    let socket = VeqSocket::bind_with_key(format!("0.0.0.0:{}", opts.listen_port), private_key)
        .await
        .unwrap();
    let connection_manager =
        ConnectionManager::new(socket.connection_info(), client, onion_address.clone(), db);
    println!("Own address: {:?}", onion_address);
    let connection_manager_arc = Arc::new(connection_manager);

    let connection_manager_arc_clone = connection_manager_arc.clone();
    tokio::spawn(
        async move { start_coordinator(coordinator_port, connection_manager_arc_clone).await },
    );

    let (sender, user_action_receiver, handle) = if !opts.no_tui {
        let (x, y, z) = insanity_tui::start_tui().await.unwrap();
        x.send(AppEvent::SetOwnAddress(onion_address.to_string()))
            .unwrap();
        (Some(x), Some(y), Some(z))
    } else {
        (None, None, None)
    };

    let peer_list = opts.peer.clone();
    let denoise = opts.denoise;

    let managed_peers = Arc::new(
        peer_list
            .iter()
            .map(|addr| OnionAddress::new(addr.clone()).unwrap())
            .zip(std::iter::repeat((
                socket.clone(),
                connection_manager_arc,
                sender.clone(),
            )))
            .map(|(peer, (socket, conn_manager, sender))| async move {
                (
                    peer.clone().to_string(),
                    ManagedPeer::new(peer, denoise, socket, conn_manager, sender).await,
                )
            })
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
