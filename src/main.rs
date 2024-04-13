use std::{path::PathBuf, str::FromStr, time::Duration};

use clap::Parser;
use insanity::connection_manager::ConnectionManager;
use insanity_tui::AppEvent;

#[derive(Parser, Debug)]
#[clap(version = "0.1.0", author = "Nicolas Chan <nicolas@nicolaschan.com>")]
struct Opts {
    #[clap(short, long, default_value = "1337")]
    listen_port: u16,

    /// Address of peer to connect to.
    #[clap(short, long)]
    peer: Vec<String>,

    /// Disables the terminal user interface.
    #[clap(long)]
    no_tui: bool,

    /// Directory to store insanity data.
    #[clap(long)]
    dir: Option<String>,

    /// Iroh join ticket
    #[clap(long)]
    room: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts: Opts = Opts::parse();

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

    let display_name = format!(
        "{}@{}",
        whoami::username(),
        whoami::fallible::hostname().unwrap_or(String::from("unknown"))
    );

    let (app_event_sender, user_action_receiver, handle) = if !opts.no_tui {
        let (x, y, z) = insanity_tui::start_tui().await.unwrap();
        // TODO: set this to a write doc token for the room.
        x.send(AppEvent::SetOwnAddress(
            opts.room.clone().unwrap_or("its me, roomless".to_string()),
        ))?;
        x.send(AppEvent::SetOwnDisplayName(display_name.clone()))?;
        (Some(x), Some(y), Some(z))
    } else {
        (None, None, None)
    };

    // Start connection manager
    let mut conn_manager_builder =
        ConnectionManager::builder(insanity_dir, opts.listen_port).display_name(display_name);
    if let Some(room) = opts.room {
        conn_manager_builder = conn_manager_builder.room(room);
    }
    if let Some(app_event_sender) = app_event_sender {
        conn_manager_builder = conn_manager_builder.app_event_sender(app_event_sender)
    }
    let connection_manager = conn_manager_builder.start().await?;

    if let Some(mut user_action_rx) = user_action_receiver {
        // Forward user actions to connection manager.
        tokio::spawn(async move {
            while let Some(action) = user_action_rx.recv().await {
                if let Err(e) = connection_manager.send_user_action(action) {
                    log::debug!("Failed to send user action to connection manager: {:?}", e);
                }
            }
        });
    }

    if let Some(handle) = handle {
        insanity_tui::stop_tui(handle).await.unwrap();
    } else {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    Ok(())


    // if let Some(mut receiver) = user_action_receiver {
    //     tokio::spawn(async move {
    //         while let Some(action) = receiver.recv().await {
    //             match action {
    //                 UserAction::DisableDenoise(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.set_denoise(false).await;
    //                     }
    //                 }
    //                 UserAction::EnableDenoise(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.set_denoise(true).await;
    //                     }
    //                 }
    //                 UserAction::DisablePeer(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.disable().await;
    //                     }
    //                 }
    //                 UserAction::EnablePeer(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.enable().await;
    //                     }
    //                 }
    //                 UserAction::SetVolume(id, volume) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.set_volume(volume).await;
    //                     }
    //                 }
    //                 UserAction::SendMessage(message) => {
    //                     for (_, peer) in managed_peers_clone.iter() {
    //                         peer.send_message(message.clone());
    //                     }
    //                 }
    //             }
    //         }
    //     });
    // }

    
    // let connection_manager = ConnectionManagerOld::new(
    //     socket.connection_info(),
    //     tor_client,
    //     onion_address.clone(),
    //     db,
    // );
    // let connection_manager_arc = Arc::new(connection_manager);

    // let connection_manager_arc_clone = connection_manager_arc.clone();
    // let name_copy = display_name.clone();

    // let (app_event_sender, user_action_receiver, handle) = if !opts.no_tui {
    //     let (x, y, z) = insanity_tui::start_tui().await.unwrap();
    //     x.send(AppEvent::SetOwnAddress(onion_address.to_string()))
    //         .unwrap();
    //     x.send(AppEvent::SetOwnDisplayName(display_name)).unwrap();
    //     (Some(x), Some(y), Some(z))
    // } else {
    //     (None, None, None)
    // };

    // let peer_list = opts.peer;
    // let denoise = opts.denoise;

    // let managed_peers = Arc::new(
    //     peer_list
    //         .into_iter()
    //         .map(|addr| OnionAddress::new(addr))
    //         .zip(std::iter::repeat((
    //             socket.clone(),
    //             connection_manager_arc,
    //             app_event_sender.clone(),
    //         )))
    //         .map(
    //             |(peer, (socket, conn_manager, app_event_sender))| async move {
    //                 (
    //                     peer.clone().to_string(),
    //                     ManagedPeerOld::new(
    //                         peer,
    //                         denoise,
    //                         100,
    //                         socket,
    //                         conn_manager,
    //                         app_event_sender,
    //                     )
    //                     .await,
    //                 )
    //             },
    //         )
    //         .collect::<FuturesUnordered<_>>()
    //         .collect::<BTreeMap<_, _>>()
    //         .await,
    // );

    // let managed_peers_clone = managed_peers.clone();
    // if let Some(mut receiver) = user_action_receiver {
    //     tokio::spawn(async move {
    //         while let Some(action) = receiver.recv().await {
    //             match action {
    //                 UserAction::DisableDenoise(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.set_denoise(false).await;
    //                     }
    //                 }
    //                 UserAction::EnableDenoise(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.set_denoise(true).await;
    //                     }
    //                 }
    //                 UserAction::DisablePeer(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.disable().await;
    //                     }
    //                 }
    //                 UserAction::EnablePeer(id) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.enable().await;
    //                     }
    //                 }
    //                 UserAction::SetVolume(id, volume) => {
    //                     if let Some(peer) = managed_peers_clone.get(&id) {
    //                         peer.set_volume(volume).await;
    //                     }
    //                 }
    //                 UserAction::SendMessage(message) => {
    //                     for (_, peer) in managed_peers_clone.iter() {
    //                         peer.send_message(message.clone());
    //                     }
    //                 }
    //             }
    //         }
    //     });
    // }

    // for peer in managed_peers.values() {
    //     peer.enable().await;
    // }

    // if let Some(handle) = handle {
    //     insanity_tui::stop_tui(handle).await.unwrap();
    // } else {
    //     loop {
    //         tokio::time::sleep(Duration::from_secs(1)).await;
    //     }
    // }

}
