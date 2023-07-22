use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::collections::BTreeMap;
use std::{error::Error, io, io::Stdout};
use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tui::{backend::Backend, backend::CrosstermBackend, Terminal};

mod editor;
use editor::Editor;
mod render;

const TAB_NAME_PEERS: &str = "Peers";
const TAB_NAME_CHAT: &str = "Chat";
const TAB_NAME_SETTINGS: &str = "Settings";

// Order must match in TAB_NAMES.
pub const TAB_IDX_PEERS: usize = 0;
pub const TAB_IDX_CHAT: usize = 1;
pub const TAB_IDX_SETTINGS: usize = 2;

pub const TOGGLE_PEER_KEY: char = ' ';
pub const TOGGLE_PEER_DENOISE_KEY: char = 'd';
pub const INCREMENT_PEER_VOLUME_KEY: char = '+';
pub const DECREMENT_PEER_VOLUME_KEY: char = '-';
pub const MOVE_DOWN_PEER_LIST_KEY: char = 'j';
pub const MOVE_UP_PEER_LIST_KEY: char = 'k';
pub const MOVE_TOP_PEER_LIST_KEY: char = 'g';
pub const MOVE_BOTTOM_PEER_LIST_KEY: char = 'G';


const NUM_TABS: usize = 3;
const TAB_NAMES: [&str; NUM_TABS] = [TAB_NAME_PEERS, TAB_NAME_CHAT, TAB_NAME_SETTINGS];


#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct ChannelId(u8);

impl Default for ChannelId {
    fn default() -> Self {
        ChannelId(0)
    }
}

impl ChannelId {
    pub fn new(id: u8) -> ChannelId {
        ChannelId(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerState {
    Connected(String),
    Disconnected,
    Disabled,
    Connecting(String),
}

#[derive(Debug, Clone)]
pub struct Peer {
    id: String,
    display_name: Option<String>,
    state: PeerState,
    channels: BTreeMap<ChannelId, Channel>,
}

#[derive(Debug, Clone)]
pub struct Channel {
    name: String,
    denoised: bool,
    volume: usize,
}

impl Default for Channel {
    fn default() -> Self {
        Channel {
            name: "Default".to_string(),
            denoised: true,
            volume: 100,
        }
    }
}

impl Channel {
    pub fn new(name: String, denoised: bool, volume: usize) -> Channel {
        Channel {
            name,
            denoised,
            volume,
        }
    }
}

impl Peer {
    pub fn new(
        id: String,
        display_name: Option<String>,
        state: PeerState,
    ) -> Peer {
        let mut channels = BTreeMap::new();
        channels.insert(ChannelId::default(), Channel::default());
        Peer {
            id,
            display_name,
            state,
            channels,
        }
    }

    pub fn add_channel(&mut self, id: ChannelId, channel: Channel) {
        self.channels.insert(id, channel);
    }

    pub fn with_state(self, state: PeerState) -> Peer {
        Peer { state, ..self }
    }

    pub fn is_default_channel_denoised(&self) -> bool {
        let default = self.channels.get(&ChannelId::default()).unwrap();
        default.denoised
    }

    pub fn get_default_channel_volume(&self) -> usize {
        let default = self.channels.get(&ChannelId::default()).unwrap();
        default.volume
    }

    pub fn set_default_channel_denoise(&mut self, denoised: bool) {
        let default = self.channels.get_mut(&ChannelId::default()).unwrap();
        default.denoised = denoised;
    }

    pub fn set_default_channel_volume(&mut self, volume: usize) {
        let default = self.channels.get_mut(&ChannelId::default()).unwrap();
        default.volume = volume;
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Kill,
    NextTab,
    PreviousTab,
    Nothing,
    Character(char),
    Enter,
    NewMessage(String, String),
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
    SetOwnDisplayName(String),
    Down,
    Up,
    TogglePeer,
    ToggleDenoise,
    SetPeerDenoise(String, bool),
    SetPeerVolume(String, usize),
}

#[derive(Debug, PartialEq, Eq)]
pub enum UserAction {
    DisablePeer(String),
    EnablePeer(String),
    DisableDenoise(String),
    EnableDenoise(String),
    SetVolume(String, usize),
    SendMessage(String),
}

pub struct App {
    pub user_action_sender: UnboundedSender<UserAction>,
    pub tabs: [&'static str; NUM_TABS],
    pub tab_index: usize,
    pub killed: bool,
    pub peers: BTreeMap<String, Peer>, // (Onion Address, Peer)
    pub own_address: Option<String>,
    pub own_display_name: Option<String>,
    pub editor: Editor,
    pub peer_index: usize,
    pub channel_index: usize,
    pub chat_history: Vec<(String, String)>, // (Display Name, Message)
    pub unread_messages: bool,
    pub chat_offset: usize, // Offset from bottom of chat in full messages.
}

impl App {
    pub fn new(sender: UnboundedSender<UserAction>) -> App {
        App {
            user_action_sender: sender,
            tabs: TAB_NAMES,
            tab_index: 0,
            killed: false,
            peers: BTreeMap::new(),
            own_address: None,
            own_display_name: None,
            editor: Editor::new(),
            peer_index: 0,
            channel_index: 0,
            chat_history: vec![],
            unread_messages: false,
            chat_offset: 0,
        }
    }

    fn num_channels_in_selected_peer(&self) -> usize {
        let peer = self.selected_peer();
        peer.map(|p| p.channels.len()).unwrap_or(0)
    }

    fn process_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Kill => {
                self.killed = true;
            }
            AppEvent::Nothing => {}
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
            AppEvent::Character(c) => match self.tab_index {
                TAB_IDX_PEERS => match c {
                    TOGGLE_PEER_KEY => {
                        self.toggle_peer();
                    }
                    TOGGLE_PEER_DENOISE_KEY => {
                        self.toggle_denoise();
                    }
                    INCREMENT_PEER_VOLUME_KEY => {
                        self.adjust_volume(1);
                    }
                    DECREMENT_PEER_VOLUME_KEY => {
                        self.adjust_volume(-1);
                    }
                    MOVE_DOWN_PEER_LIST_KEY => {
                        self.move_peer(1);
                    }
                    MOVE_UP_PEER_LIST_KEY => {
                        self.move_peer(-1);
                    }
                    MOVE_TOP_PEER_LIST_KEY => {
                        self.peer_index = 0;
                        self.channel_index = 0;
                    }
                    MOVE_BOTTOM_PEER_LIST_KEY => {
                        self.peer_index = self.peers.len() - 1;
                        self.channel_index = self.num_channels_in_selected_peer() - 1;
                    }
                    _ => {}
                },
                TAB_IDX_CHAT => {
                    self.editor.append(c);
                }
                _ => {}
            },
            AppEvent::Enter => match self.tab_index {
                TAB_IDX_CHAT => {
                    self.send_message();
                }
                _ => {}
            },
            AppEvent::NewMessage(sender_name, message) => {
                self.add_message((sender_name, message));
                if self.tab_index != TAB_IDX_CHAT || self.chat_offset > 0 {
                    self.unread_messages = true;
                }
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
            AppEvent::SetOwnDisplayName(display_name) => {
                self.own_display_name = Some(display_name);
            }
            AppEvent::Down => match self.tab_index {
                TAB_IDX_PEERS => {
                    if self.channel_index + 1 < self.num_channels_in_selected_peer() {
                        self.channel_index += 1;
                    } else {
                        self.channel_index = 0;
                        self.peer_index = std::cmp::min(
                            self.peer_index.checked_add(1).unwrap_or(0),
                            self.peers.len() - 1,
                        );
                    }
                }
                TAB_IDX_CHAT => {
                    self.chat_offset = self.chat_offset.saturating_sub(1);
                    if self.chat_offset == 0 {
                        self.unread_messages = false;
                    }
                }
                _ => {}
            },
            AppEvent::Up => match self.tab_index {
                TAB_IDX_PEERS => {
                    if self.channel_index > 0 {
                        self.channel_index -= 1;
                    } else {
                        self.channel_index = self.num_channels_in_selected_peer() - 1;
                        self.peer_index = self.peer_index.saturating_sub(1);
                    }
                }
                TAB_IDX_CHAT => {
                    self.chat_offset = std::cmp::min(self.chat_history.len(), self.chat_offset + 1);
                }
                _ => {}
            },
            AppEvent::TogglePeer => {
                self.toggle_peer();
            }
            AppEvent::ToggleDenoise => {
                self.toggle_denoise();
            }
            AppEvent::SetPeerDenoise(peer_id, denoised) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.set_default_channel_denoise(denoised);
                }
            }
            AppEvent::SetPeerVolume(peer_id, volume) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.set_default_channel_volume(volume);
                }
            }
        }
    }

    fn move_peer(&mut self, delta: isize) {
        self.peer_index = add_in_bounds(self.peer_index, 0, self.peers.len() - 1, delta);
    }

    fn selected_peer(&self) -> Option<&Peer> {
        self.peers.values().nth(self.peer_index)
    }

    fn toggle_peer(&mut self) {
        if let Some(peer) = self.selected_peer() {
            if peer.state == PeerState::Disabled {
                self.user_action_sender
                    .send(UserAction::EnablePeer(peer.id.clone()))
                    .unwrap();
            } else {
                self.user_action_sender
                    .send(UserAction::DisablePeer(peer.id.clone()))
                    .unwrap();
            }
        }
    }

    fn add_message(&mut self, message: (String, String)) {
        self.chat_history.push(message);
        // If offset to a particular message, stay offset to that message.
        // Assume offset of 0 means scroll with new messages.
        if self.chat_offset > 0 {
            self.chat_offset += 1;
        }
    }

    fn send_message(&mut self) {
        if !self.editor.is_empty() {
            let message = self.editor.clear();
            let default = "Me".to_string();
            let own_address = self.own_address.clone().unwrap_or(default);
            self.add_message((own_address, message.clone()));
            self.user_action_sender
                .send(UserAction::SendMessage(message))
                .unwrap();
        }
    }

    fn toggle_denoise(&mut self) {
        if let Some(peer) = self.selected_peer() {
            if peer.is_default_channel_denoised() {
                self.user_action_sender
                    .send(UserAction::DisableDenoise(peer.id.clone()))
                    .unwrap();
            } else {
                self.user_action_sender
                    .send(UserAction::EnableDenoise(peer.id.clone()))
                    .unwrap();
            }
        }
    }

    fn adjust_volume(&mut self, delta: isize) {
        if let Some(peer) = self.selected_peer() {
            self.user_action_sender
                .send(UserAction::SetVolume(
                    peer.id.clone(),
                    add_in_bounds(peer.get_default_channel_volume(), 0, 999, delta),
                ))
                .unwrap();
        }
    }

    fn move_tabs(&mut self, adjustment: isize) {
        let num_tabs = self.tabs.len();
        self.tab_index = (self.tab_index + adjustment.rem_euclid(num_tabs as isize) as usize)
            .rem_euclid(num_tabs);
        if self.tab_index == TAB_IDX_CHAT && self.chat_offset == 0 {
            self.unread_messages = false;
        }
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
                        KeyCode::Down => {
                            sender.send(AppEvent::Down).unwrap();
                        }
                        KeyCode::Up => {
                            sender.send(AppEvent::Up).unwrap();
                        }
                        KeyCode::Enter => {
                            sender.send(AppEvent::Enter).unwrap();
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
        UnboundedReceiver<UserAction>,
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

    let (app_user_action_sender, app_user_action_receiver) = unbounded_channel();

    let app = App::new(app_user_action_sender);
    let (app_event_sender, handle) = get_sender(app, terminal).await;
    handle_input(app_event_sender.clone()).await;
    Ok((app_event_sender, app_user_action_receiver, handle))
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

fn add_in_bounds(value: usize, min: usize, max: usize, delta: isize) -> usize {
    let new_value = value as isize + delta;
    if new_value < min as isize {
        min
    } else if new_value > max as isize {
        max
    } else {
        new_value as usize
    }
}
