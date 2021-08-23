use std::collections::HashMap;
use std::default::Default;

use crossterm::event::{read, Event, KeyCode, KeyModifiers, KeyEvent};
use tui::Terminal;
use tui::widgets::{Widget, Block, Borders};
use tui::backend::CrosstermBackend;
use tui::layout::{Layout, Constraint, Direction};

pub struct InsanityTui {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

#[derive(Clone, Default)]
struct TuiStatus {
    peers: HashMap<String, Peer>,
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
enum TuiEvent {
    Interaction(Event),
    Message(TuiMessage),
}

#[derive(Eq, PartialEq, Clone)]
enum TuiMessage {
    UpdatePeer(String, Peer),
    DeletePeer(String),
}

#[derive(Eq, PartialEq, Clone)]
struct Peer {
    ip_address: String,
}

impl InsanityTui {
    fn draw_dashboard(&mut self, status: &TuiStatus) {
        self.terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(100),
                ].as_ref())
                .split(f.size());
            let size = f.size();
            let block = Block::default()
                .title("Peers")
                .borders(Borders::ALL);
            f.render_widget(block, chunks[0]);
        }).unwrap();
    }

    fn redraw(&mut self, state: &TuiState) {
        match &state.route {
            Dashboard => self.draw_dashboard(&state.status),
        }
    }

    // key events update the TuiState

}

fn next_state(event: Event, state: TuiState) -> TuiState {
    if event == Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
        return TuiState {
            route: TuiRoute::Killed,
            status: state.status,
        }
    }
    state
}

pub fn start() {
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("Could not open terminal");
    crossterm::terminal::enable_raw_mode().unwrap();

    let mut tui = InsanityTui { terminal };

    let mut peers = HashMap::new();
    peers.insert("bruh".to_string(), Peer { ip_address: "bruh.com".to_string() });

    let mut state = TuiState {
        route: TuiRoute::Dashboard,
        status: TuiStatus { peers }
    };

    loop {
        tui.redraw(&state);
        let event = read().unwrap(); // blocking
        state = next_state(event, state);
        if &state.route == &TuiRoute::Killed {
            break;
        }
    }
}
