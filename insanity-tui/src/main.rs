use insanity_tui::{start_tui, stop_tui, AppEvent, Peer, PeerState};
use std::{error::Error};


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (sender, handle) = start_tui().await?;
    sender
        .send(AppEvent::AddPeer(Peer::new(
            "francis".to_string(),
            None,
            PeerState::Disconnected,
        )))
        .unwrap();
    sender
        .send(AppEvent::AddPeer(Peer::new(
            "nicolas".to_string(),
            Some("nicolas".to_string()),
            PeerState::Connected("hi".to_string()),
        )))
        .unwrap();
    stop_tui(handle).await?;
    Ok(())
}
