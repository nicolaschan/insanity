use std::{path::PathBuf, str::FromStr, sync::{Arc}, thread::{self, JoinHandle}, time::Duration};

use clap::{AppSettings, Clap};
use crossbeam::channel::{unbounded};
use insanity::{InsanityConfig, coordinator::{start_coordinator, start_tor}, protocol::{ConnectionManager}, tui::{Peer, PeerStatus, TuiEvent, TuiMessage}};


#[derive(Clap)]
#[clap(version = "0.1.0", author = "Nicolas Chan <nicolas@nicolaschan.com>")]
#[clap(setting = AppSettings::ColoredHelp)]
struct Opts {
    #[clap(short, long)]
    denoise: bool,

    #[clap(long)]
    music: Option<String>,

    #[clap(short, long, default_value = "1337")]
    listen_port: u16,

    #[clap(short, long)]
    peer_address: Vec<String>,

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

#[tokio::main]
async fn main() {
    let opts: Opts = Opts::parse();

    let _id: String = match opts.id {
        Some(id) => id,
        None => nanoid::nanoid!(),
    };

    let insanity_dir = match opts.dir {
        Some(dir) => PathBuf::from_str(&dir).unwrap(),
        None => dirs::data_local_dir().expect("no data directory!?").join("insanity"),
    };
    std::fs::create_dir_all(&insanity_dir).expect("could not create insanity data directory");

    let tor_dir = insanity_dir.join("tor");
    std::fs::create_dir_all(&tor_dir).expect("could not create tor data directory");
    let socks_port = opts.socks_port;
    let coordinator_port = opts.coordinator_port;
    let onion_address= start_tor(&tor_dir, socks_port, coordinator_port);

    let proxy = reqwest::Proxy::all(format!("socks5h://127.0.0.1:{}", socks_port)).unwrap();
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .build()
        .unwrap();

    let mut connection_manager = ConnectionManager::new(&onion_address, client);
    let udp = connection_manager.create_server_socket(opts.listen_port);
    let connection_manager_arc= Arc::new(connection_manager);
    let connection_manager_arc_clone = connection_manager_arc.clone();
    thread::spawn(move || start_coordinator(coordinator_port, connection_manager_arc_clone));

    let (ui_message_sender, ui_message_receiver) = unbounded();

    let config = InsanityConfig {
        denoise: opts.denoise,
        ui_message_sender: ui_message_sender.clone(),
        music: opts.music,
        sample_rate: opts.sample_rate,
        channels: opts.channels,
    };

    let mut tui_join_handle: Option<JoinHandle<()>> = None;
    if ! opts.no_tui {
        let ui_message_sender_clone = ui_message_sender.clone();
        let ui_message_receiver_clone = ui_message_receiver.clone();
        tui_join_handle = Some(thread::spawn(move || insanity::tui::start(ui_message_sender_clone, ui_message_receiver_clone)));
        ui_message_sender.send(TuiEvent::Message(TuiMessage::SetOwnAddress(Some(onion_address.clone())))).unwrap();
    }

    let config_clone = config.clone();
    thread::spawn(move || {
        insanity::server::start_server(udp, config_clone);
    });

    for peer in opts.peer {
        ui_message_sender.send(TuiEvent::Message(TuiMessage::UpdatePeer(
            peer.clone(),
            Peer {
                name: peer.clone(),
                status: PeerStatus::Disconnected,
            },
        ))).unwrap();
        let addresses = connection_manager_arc.add_peer(&peer).await;
        for address in addresses {
            let config_clone = config.clone();
            let peer_clone = peer.clone();
            let onion_address_clone = onion_address.clone();
            thread::spawn(move || {
                insanity::client::start_client(onion_address_clone, peer_clone, address, config_clone)
            });
        }
    }

    // for peer_address in opts.peer_address {
    //     let config_clone = config.clone();
    //     thread::spawn(move || {
    //         insanity::client::start_client(peer_address, config_clone);
    //     });
    // }

    match tui_join_handle {
        Some(handle) => { handle.join().unwrap(); },
        None => {
            loop {
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    }
}
