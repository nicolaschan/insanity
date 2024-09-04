use insanity_core::user_input_event::UserInputEvent;
use insanity_tui_adapter::{start_tui, stop_tui, AppEvent, Peer, PeerState};
use std::{collections::BTreeMap, error::Error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (sender, mut user_action_receiver, handle) = start_tui().await?;
    let mut peers = BTreeMap::new();
    peers.insert(
        "francis",
        Peer::new(
            "francis".to_string(),
            None,
            PeerState::Disconnected,
            true,
            100,
        ),
    );
    peers.insert(
        "nicolas",
        Peer::new(
            "nicolas".to_string(),
            None,
            PeerState::Connected("hi".to_string()),
            false,
            100,
        ),
    );
    peers.insert(
        "randall",
        Peer::new("randall".to_string(), None, PeerState::Disabled, true, 100),
    );
    peers.insert(
        "neelay",
        Peer::new(
            "neelay".to_string(),
            None,
            PeerState::Connecting("bruh".to_string()),
            true,
            100,
        ),
    );

    for peer in peers.values() {
        sender.send(AppEvent::AddPeer(peer.clone())).unwrap();
    }

    tokio::spawn(async move {
        while let Some(event) = user_action_receiver.recv().await {
            match event {
                UserInputEvent::EnableDenoise(peer_id) => {
                    sender
                        .send(AppEvent::SetPeerDenoise(peer_id, true))
                        .unwrap();
                }
                UserInputEvent::DisableDenoise(peer_id) => {
                    sender
                        .send(AppEvent::SetPeerDenoise(peer_id, false))
                        .unwrap();
                }
                UserInputEvent::DisablePeer(peer_id) => {
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
                UserInputEvent::EnablePeer(peer_id) => {
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
                UserInputEvent::SetVolume(peer_id, volume) => {
                    sender
                        .send(AppEvent::SetPeerVolume(peer_id, volume))
                        .unwrap();
                }
                UserInputEvent::SendMessage(_message) => {}
                UserInputEvent::SetMuteSelf(_) => todo!(),
            }
        }
    });
    stop_tui(handle).await?;
    Ok(())
}
