use insanity_tui::{start_tui, Peer, PeerState, ChannelId, Channel, AppEvent, UserAction, stop_tui};
use std::{collections::BTreeMap, error::Error};


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (sender, mut user_action_receiver, handle) = start_tui().await?;
    let mut peers: BTreeMap<&str, Peer> = BTreeMap::new();
    peers.insert(
        "francis",
        Peer::new(
            "francis".to_string(),
            None,
            PeerState::Disconnected
        ),
    );
    peers.insert(
        "nicolas",
        Peer::new(
            "nicolas".to_string(),
            None,
            PeerState::Connected("hi".to_string())
        ),
    );
    peers.insert(
        "randall",
        Peer::new("randall".to_string(), None, PeerState::Disabled),
    );
    peers.insert(
        "neelay",
        Peer::new(
            "neelay".to_string(),
            None,
            PeerState::Connecting("bruh".to_string()),
        ),
    );

    let nicolas = peers.get_mut("nicolas").unwrap();
    nicolas.add_channel(ChannelId::new(8u8), Channel::new("music".to_string(), true, 100));
    nicolas.add_channel(ChannelId::new(9u8), Channel::new("game".to_string(), true, 100));

    for peer in peers.values() {
        sender.send(AppEvent::AddPeer(peer.clone())).unwrap();
    }

    tokio::spawn(async move {
        while let Some(event) = user_action_receiver.recv().await {
            match event {
                UserAction::EnableDenoise(peer_id) => {
                    sender
                        .send(AppEvent::SetPeerDenoise(peer_id, true))
                        .unwrap();
                }
                UserAction::DisableDenoise(peer_id) => {
                    sender
                        .send(AppEvent::SetPeerDenoise(peer_id, false))
                        .unwrap();
                }
                UserAction::DisablePeer(peer_id) => {
                    sender
                        .send(AppEvent::AddPeer(
                            peers
                                .get(&peer_id.as_str())
                                .unwrap()
                                .clone()
                                .with_state(PeerState::Disabled),
                        ))
                        .unwrap();
                }
                UserAction::EnablePeer(peer_id) => {
                    sender
                        .send(AppEvent::AddPeer(
                            peers
                                .get(&peer_id.as_str())
                                .unwrap()
                                .clone()
                                .with_state(PeerState::Disconnected),
                        ))
                        .unwrap();
                }
                UserAction::SetVolume(peer_id, volume) => {
                    sender
                        .send(AppEvent::SetPeerVolume(peer_id, volume))
                        .unwrap();
                }
                UserAction::SendMessage(_message) => {}
            }
        }
    });
    stop_tui(handle).await?;
    Ok(())
}
