use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use clap::Parser;
use futures_util::stream::FuturesUnordered;
use insanity::{
    clerver::start_clerver,
    coordinator::{start_coordinator, start_tor},
    protocol::{ConnectionManager, OnionAddress},
};
use insanity_tui::{AppEvent, Peer, PeerState};
use std::iter::Iterator;

use veq::veq::{VeqSocket};
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

    let socket = VeqSocket::bind(format!("0.0.0.0:{}", opts.listen_port))
        .await
        .unwrap();
    let connection_manager =
        ConnectionManager::new(socket.connection_info(), client, onion_address.clone());
    println!("Own address: {:?}", onion_address);
    let connection_manager_arc = Arc::new(connection_manager);

    let connection_manager_arc_clone = connection_manager_arc.clone();
    tokio::spawn(
        async move { start_coordinator(coordinator_port, connection_manager_arc_clone).await },
    );

    let (sender, handle) = if !opts.no_tui {
        let (x, y) = insanity_tui::start_tui().await.unwrap();
        (Some(x), Some(y))
    } else {
        (None, None)
    };

    let peer_list = opts.peer.clone();
    let denoise = opts.denoise;
    tokio::spawn(async move {
        peer_list
        .iter()
        .map(|addr| OnionAddress::new(addr.clone()).unwrap())
        .zip(std::iter::repeat((socket.clone(), denoise, connection_manager_arc, sender.clone())))
        .map(|(peer, (mut socket, denoise, conn_manager, sender))| async move {
            loop {
                if let Some(sender) = sender.clone() { sender
                        .send(AppEvent::AddPeer(Peer::new(
                            peer.to_string(),
                            PeerState::Disconnected,
                        )))
                        .unwrap(); }
                if let Some(session) = conn_manager.session(&mut socket, &peer).await {
                    if let Some(sender) = sender.clone() { sender
                            .send(AppEvent::AddPeer(Peer::new(
                                peer.to_string(),
                                PeerState::Connected("hi".to_string()),
                            )))
                            .unwrap(); }
                    start_clerver(session, denoise).await;
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