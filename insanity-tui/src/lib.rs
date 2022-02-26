use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{cmp::min, collections::HashMap, error::Error, io, io::Stdout};
use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tui::{backend::Backend, backend::CrosstermBackend, Terminal};

mod render;

#[derive(Debug, Clone)]
pub enum PeerState {
    Connected(String),
    Disconnected,
}

#[derive(Debug)]
pub struct Peer {
    id: String,
    display_name: Option<String>,
    state: PeerState,
}

impl Peer {
    pub fn new(id: String, display_name: Option<String>, state: PeerState) -> Peer {
        Peer { id, display_name, state }
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Kill,
    NextTab,
    PreviousTab,
    Nothing,
    Character(char),
    AddPeer(Peer),
    RemovePeer(String),
    Backspace,
    Left,
    Right,
    CursorBeginning,
    CursorEnd,
    PreviousWord,
    NextWord,
    DeleteWord,
    SetOwnAddress(String),
}

pub struct Editor {
    pub buffer: String,
    pub cursor: usize,
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            buffer: String::new(),
            cursor: 0,
        }
    }

    pub fn append(&mut self, c: char) {
        let mut chars: Vec<char> = self.buffer.chars().collect();
        chars.insert(self.cursor, c);
        self.buffer = chars.iter().collect();
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if let Some(val) = self.cursor.checked_sub(1) {
            let mut chars: Vec<char> = self.buffer.chars().collect();
            chars.remove(self.cursor.saturating_sub(1));
            self.buffer = chars.iter().collect();
            self.cursor = val;
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = min(self.cursor + 1, self.buffer.len());
    }

    pub fn cursor_beginning(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    pub fn delete_word(&mut self) {
        for _ in 0..self
            .cursor.saturating_sub(self.previous_word_index())
        {
            self.backspace();
        }
    }

    pub fn next_word(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut found = false;
        for i in (self.cursor + 1)..self.buffer.chars().collect::<Vec<char>>().len() {
            if found {
                if let Some(' ') = chars.get(i) {
                    self.cursor = i;
                    return;
                }
            } else if let Some(c) = chars.get(i) {
                if c != &' ' {
                    found = true;
                }
            }
        }
        self.cursor = self.buffer.chars().collect::<Vec<char>>().len();
    }

    fn previous_word_index(&mut self) -> usize {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut found = false;
        for i in (0..self.cursor.saturating_sub(1)).rev() {
            if found {
                if let Some(' ') = chars.get(i) {
                    return i + 1;
                }
            } else if let Some(c) = chars.get(i) {
                if c != &' ' {
                    found = true;
                }
            }
        }
        0
    }

    pub fn previous_word(&mut self) {
        self.cursor = self.previous_word_index();
    }
}

pub struct App {
    pub tabs: Vec<String>,
    pub tab_index: usize,
    pub killed: bool,
    pub peers: HashMap<String, Peer>,
    pub own_address: Option<String>,
    pub editor: Editor,
}

impl App {
    pub fn new() -> App {
        App {
            tabs: ["Peers", "Chat", "Settings"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tab_index: 0,
            killed: false,
            peers: HashMap::new(),
            own_address: None,
            editor: Editor::new(),
        }
    }

    fn process_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Kill => {
                self.killed = true;
            }
            AppEvent::NextTab => {
                self.move_tabs(1);
            }
            AppEvent::PreviousTab => {
                self.move_tabs(-1);
            }
            AppEvent::AddPeer(peer) => {
                self.peers.insert(peer.id.clone(), peer);
            }
            AppEvent::RemovePeer(id) => {
                self.peers.remove(&id);
            }
            AppEvent::Character(c) => {
                self.editor.append(c);
            }
            AppEvent::Backspace => {
                self.editor.backspace();
            }
            AppEvent::Left => {
                self.editor.left();
            }
            AppEvent::Right => {
                self.editor.right();
            }
            AppEvent::CursorBeginning => {
                self.editor.cursor_beginning();
            }
            AppEvent::CursorEnd => {
                self.editor.cursor_end();
            }
            AppEvent::PreviousWord => {
                self.editor.previous_word();
            }
            AppEvent::NextWord => {
                self.editor.next_word();
            }
            AppEvent::DeleteWord => {
                self.editor.delete_word();
            }
            AppEvent::SetOwnAddress(address) => {
                self.own_address = Some(address);
            }
            _ => {}
        }
    }

    fn move_tabs(&mut self, adjustment: isize) {
        let num_tabs = self.tabs.len();
        self.tab_index = (self.tab_index + adjustment.rem_euclid(num_tabs as isize) as usize)
            .rem_euclid(num_tabs);
    }

    pub fn render<B: Backend>(&self, terminal: &mut Terminal<B>) -> io::Result<bool> {
        terminal.draw(|f| render::ui(f, self)).unwrap();
        Ok(self.killed)
    }
}

pub async fn get_sender<B: Backend + Send + 'static>(
    mut app: App,
    mut terminal: Terminal<B>,
) -> (UnboundedSender<AppEvent>, JoinHandle<Terminal<B>>) {
    let (sender, mut receiver): (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) =
        unbounded_channel();
    let handle = tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            app.process_event(event);
            if let Ok(true) = app.render(&mut terminal) {
                break;
            }
        }
        terminal
    });
    sender.send(AppEvent::Nothing).unwrap();
    (sender, handle)
}

pub async fn handle_input(sender: UnboundedSender<AppEvent>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || loop {
        match event::read().unwrap() {
            Event::Key(key) => {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    match key.code {
                        KeyCode::Char(c) => {
                            sender.send(AppEvent::Character(c)).unwrap();
                        }
                        KeyCode::Tab => {
                            sender.send(AppEvent::NextTab).unwrap();
                        }
                        KeyCode::BackTab => {
                            sender.send(AppEvent::PreviousTab).unwrap();
                        }
                        KeyCode::Backspace => {
                            sender.send(AppEvent::Backspace).unwrap();
                        }
                        KeyCode::Left => {
                            sender.send(AppEvent::Left).unwrap();
                        }
                        KeyCode::Right => {
                            sender.send(AppEvent::Right).unwrap();
                        }
                        _ => {}
                    }
                } else {
                    if key.modifiers.contains(KeyModifiers::ALT) {
                        match key.code {
                            KeyCode::Char('f') => {
                                sender.send(AppEvent::NextWord).unwrap();
                            }
                            KeyCode::Char('b') => {
                                sender.send(AppEvent::PreviousWord).unwrap();
                            }
                            KeyCode::Backspace => {
                                sender.send(AppEvent::DeleteWord).unwrap();
                            }
                            _ => {}
                        }
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match key.code {
                            KeyCode::Char('c') => {
                                sender.send(AppEvent::Kill).unwrap();
                                return;
                            }
                            KeyCode::Char('a') => {
                                sender.send(AppEvent::CursorBeginning).unwrap();
                            }
                            KeyCode::Char('e') => {
                                sender.send(AppEvent::CursorEnd).unwrap();
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::Resize(_, _) => {
                sender.send(AppEvent::Nothing).unwrap();
            }
            _ => {}
        }
    })
}

pub async fn start_tui() -> Result<
    (
        UnboundedSender<AppEvent>,
        JoinHandle<Terminal<CrosstermBackend<Stdout>>>,
    ),
    Box<dyn Error>,
> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let app = App::new();
    let (sender, handle) = get_sender(app, terminal).await;
    handle_input(sender.clone()).await;
    Ok((sender, handle))
}

pub async fn stop_tui(
    handle: JoinHandle<Terminal<CrosstermBackend<Stdout>>>,
) -> Result<(), Box<dyn Error>> {
    let mut terminal = handle.await.unwrap();
    disable_raw_mode().unwrap();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).unwrap();
    terminal.show_cursor().unwrap();
    Ok(())
}
