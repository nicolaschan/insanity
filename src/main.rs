use std::{path::PathBuf, str::FromStr, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration, collections::HashMap};

use clap::Parser;
use futures_util::stream::FuturesUnordered;
use insanity::{
    clerver::start_clerver,
    coordinator::{start_coordinator, start_tor},
    protocol::{ConnectionManager, OnionAddress},
};
use insanity_tui::{AppEvent, Peer, PeerState, UserAction};
use tokio::sync::Mutex;
use std::iter::Iterator;

use veq::{veq::{VeqSocket}, snow_types::{SnowPrivateKey, SnowKeypair}};
use futures_util::StreamExt;

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
        .level(log::LevelFilter::Trace)
        .chain(fern::log_file(insanity_dir.join("insanity.log"))
            .expect("could not create insanity log file"))
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
            db.insert("private_key", bincode::serialize(&key).unwrap()).unwrap();
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
        x.send(AppEvent::SetOwnAddress(onion_address.to_string())).unwrap();
        (Some(x), Some(y), Some(z))
    } else {
        (None, None, None)
    };

    let peer_list = opts.peer.clone();
    let denoise = opts.denoise;

    let denoises = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut guard = denoises.lock().await;
        for peer in opts.peer.clone() {
            let denoise = Arc::new(AtomicBool::from(denoise));
            guard.insert(peer, denoise);
        }
    }

    let sender_clone = sender.clone();
    let denoises_clone = denoises.clone();
    if let Some(mut receiver) = user_action_receiver {
        tokio::spawn(async move {
            while let Some(action) = receiver.recv().await {
                match action {
                    UserAction::DisableDenoise(id) => {
                        let mut guard = denoises_clone.lock().await;
                        let denoise = guard.get_mut(&id);
                        if let Some(denoise) = denoise {
                            denoise.store(false, Ordering::Relaxed);
                            if let Some(sender) = &sender_clone {
                                sender.send(AppEvent::SetPeerDenoise(id, false)).unwrap();
                            }
                        }
                    }
                    UserAction::EnableDenoise(id) => {
                        let mut guard = denoises_clone.lock().await;
                        let denoise = guard.get_mut(&id);
                        if let Some(denoise) = denoise {
                            denoise.store(true, Ordering::Relaxed);
                            if let Some(sender) = &sender_clone {
                                sender.send(AppEvent::SetPeerDenoise(id, true)).unwrap();
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    let denoises_clone = denoises.clone();
    tokio::spawn(async move {
        peer_list
        .iter()
        .map(|addr| OnionAddress::new(addr.clone()).unwrap())
        .zip(std::iter::repeat((socket.clone(), connection_manager_arc, sender.clone(), denoises_clone)))
        .map(|(peer, (mut socket, conn_manager, sender, denoises))| async move {
            loop {
                let denoise = {
                    let guard = denoises.lock().await;
                    guard.get(&peer.to_string()).unwrap().clone()
                };
                log::info!("Connecting to {:?} with denoise value {:?}", peer, denoise.load(Ordering::Relaxed));
                if let Some(sender) = sender.clone() { sender
                        .send(AppEvent::AddPeer(Peer::new(
                            peer.to_string(),
                            None,
                            PeerState::Disconnected,
                            denoise.load(Ordering::Relaxed),
                        )))
                        .unwrap(); }
                if let Some((session, info)) = conn_manager.session(&mut socket, &peer).await {
                    log::info!("Connection established with {}", peer);
                    if let Some(sender) = sender.clone() { sender
                            .send(AppEvent::AddPeer(Peer::new(
                                peer.to_string(),
                                Some(info.display_name),
                                PeerState::Connected(session.remote_addr().await.to_string()),
                                denoise.load(Ordering::Relaxed),
                            )))
                            .unwrap(); }
                    start_clerver(session, denoise).await;
                    log::info!("Connection closed with {}", peer);
                }
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;
    });

    // let mut compressed = Vec::new();
    // zstd::stream::copy_encode(
    //     &bincode::serialize(&socket.connection_info()).unwrap()[..],
    //     &mut compressed,
    //     10,
    // )
    // .unwrap();
    // let encoded_conn_info = base65536::encode(&compressed, None);
    // println!("ConnectionInfo: {}", encoded_conn_info);
    // let stdin = std::io::stdin();
    // let mut remote_conn_info = String::new();
    // stdin.lock().read_line(&mut remote_conn_info).unwrap();
    // let remote_conn_info = remote_conn_info.replace('\n', "").replace(' ', "");
    // let remote_conn_info_compressed = &base65536::decode(&remote_conn_info, true).unwrap();
    // let mut remote_conn_info_bytes = Vec::new();
    // zstd::stream::copy_decode(
    //     &remote_conn_info_compressed[..],
    //     &mut remote_conn_info_bytes,
    // )
    // .unwrap();
    // let peer_data: ConnectionInfo = bincode::deserialize(&remote_conn_info_bytes).unwrap();
    // let conn = socket.connect(Uuid::from_u128(0), peer_data).await;
    // println!("connected");
    // start_clerver(conn, opts.denoise).await;

    if let Some(handle) = handle {
        insanity_tui::stop_tui(handle).await.unwrap();
    } else {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
