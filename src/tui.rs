use std::collections::HashMap;
use std::default::Default;
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use crossbeam::channel::{Receiver, Sender};
use crossterm::event::{read, Event, KeyCode, KeyEvent, KeyModifiers};
use itertools::Itertools;
use tui::backend::CrosstermBackend;
use tui::layout::{Constraint, Direction, Layout};
use tui::style::{Color, Style};
use tui::text::{Span, Spans};
use tui::widgets::{Block, Borders, List, ListItem};
use tui::Terminal;

pub struct InsanityTui {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

#[derive(Clone, Default)]
struct TuiStatus {
    peers: HashMap<String, Peer>,
    own_address: Option<String>,
}

#[derive(Eq, PartialEq, Clone)]
enum TuiRoute {
    Dashboard,
    Killed,
}

#[derive(Clone)]
struct TuiState {
    route: TuiRoute,
    status: TuiStatus,
}

#[derive(Eq, PartialEq, Clone)]
pub enum TuiEvent {
    Interaction(Event),
    Message(TuiMessage),
}

#[derive(Eq, PartialEq, Clone)]
pub enum TuiMessage {
    UpdatePeer(String, Peer),
    DeletePeer(String),
    SetOwnAddress(Option<String>),
}

#[derive(Eq, PartialEq, Clone)]
pub struct Peer {
    pub name: String,
    pub status: PeerStatus,
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub enum PeerStatus {
    Connected(SocketAddr),
    Disconnected,
}

fn draw_peers(peers: &HashMap<String, Peer>) -> List<'static> {
    let block = Block::default()
        .title("Peers")
        // .style(Style::default().bg(Color::Black))
        .borders(Borders::ALL);
    let peer_list_items: Vec<ListItem> = peers
        .iter()
        .sorted_by(|(id1, _), (id2, _)| id1.cmp(id2))
        .map(|(_, peer)| {
            let peer_style = match peer.status {
                PeerStatus::Connected(_socket_addr) => Style::default().fg(Color::LightGreen),
                PeerStatus::Disconnected => Style::default().fg(Color::LightRed),
            };
            let content = match peer.status {
                PeerStatus::Connected(socket_addr) => {
                    vec![Spans::from(vec![
                        Span::styled(format!("✓ {}", peer.name), peer_style),
                        Span::styled(format!(" @ {}", socket_addr), Style::default()),
                    ])]
                }
                PeerStatus::Disconnected => {
                    vec![Spans::from(vec![Span::styled(
                        format!("✗ {}", peer.name),
                        peer_style,
                    )])]
                }
            };
            ListItem::new(content)
        })
        .collect();

    List::new(peer_list_items).block(block)
}

fn draw_own_info(address: &Option<String>) -> List<'static> {
    let address = address.clone().unwrap_or_else(|| "".to_string());
    let block = Block::default().title("Your address").borders(Borders::ALL);
    List::new(vec![ListItem::new(vec![Spans::from(vec![Span::styled(
        address,
        Style::default().fg(Color::LightBlue),
    )])])])
    .block(block)
}

impl InsanityTui {
    fn draw_dashboard(&mut self, status: &TuiStatus) {
        self.terminal
            .draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
                    .split(f.size());

                let own_info = draw_own_info(&status.own_address);
                f.render_widget(own_info, chunks[0]);
                let peers_list = draw_peers(&status.peers);
                f.render_widget(peers_list, chunks[1]);
            })
            .unwrap();
    }

    fn redraw(&mut self, state: &TuiState) {
        match &state.route {
            TuiRoute::Dashboard => self.draw_dashboard(&state.status),
            TuiRoute::Killed => {}
        }
    }

    // key events update the TuiState
}

fn next_state_key_event(event: Event, state: TuiState) -> TuiState {
    if event == Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        return TuiState {
            route: TuiRoute::Killed,
            status: state.status,
        };
    }
    state
}

fn next_state_message(message: TuiMessage, mut state: TuiState) -> TuiState {
    match message {
        TuiMessage::UpdatePeer(k, v) => {
            state.status.peers.insert(k, v);
        }
        TuiMessage::DeletePeer(k) => {
            state.status.peers.remove(&k);
        }
        TuiMessage::SetOwnAddress(address) => {
            state.status.own_address = address;
        }
    }
    state
}

fn next_state(event: TuiEvent, state: TuiState) -> TuiState {
    match event {
        TuiEvent::Interaction(event) => next_state_key_event(event, state),
        TuiEvent::Message(message) => next_state_message(message, state),
    }
}

pub fn start(ui_message_sender: Sender<TuiEvent>, receiver: Receiver<TuiEvent>) {
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("Could not open terminal");
    crossterm::terminal::enable_raw_mode().unwrap();
    terminal.clear().unwrap();

    let mut tui = InsanityTui { terminal };
    let peers = HashMap::new();

    let mut state = TuiState {
        route: TuiRoute::Dashboard,
        status: TuiStatus {
            own_address: None,
            peers,
        },
    };

    thread::spawn(move || {
        loop {
            let event = read().unwrap(); // blocking
            if ui_message_sender.send(TuiEvent::Interaction(event)).is_ok() {}
        }
    });
    loop {
        while let Ok(tui_event) = receiver.try_recv() {
            state = next_state(tui_event, state);
        }
        if state.route == TuiRoute::Killed {
            if crossterm::terminal::disable_raw_mode().is_ok() {}
            break;
        }
        tui.redraw(&state);
        thread::sleep(Duration::from_millis(50));
    }
}
