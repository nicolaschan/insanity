use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use insanity_tui::{get_sender, handle_input, App, AppEvent, Peer, PeerState};
use std::{error::Error, io};
use tui::{backend::CrosstermBackend, Terminal};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let app = App::new();
    let (sender, handle) = get_sender(app, terminal).await;
    handle_input(sender.clone()).await;
    sender
        .send(AppEvent::AddPeer(Peer::new(
            "francis".to_string(),
            PeerState::Disconnected,
        )))
        .unwrap();
    sender
        .send(AppEvent::AddPeer(Peer::new(
            "nicolas".to_string(),
            PeerState::Connected("hi".to_string()),
        )))
        .unwrap();

    let mut terminal = handle.await.unwrap();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
