use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use insanity_core::user_input_event::{DenoiseSelection, UserInputEvent};
use ratatui::{DefaultTerminal, Terminal, backend::Backend, backend::CrosstermBackend};
use std::collections::BTreeMap;
use std::{error::Error, io};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    task::JoinHandle,
};

mod components;
mod editor;
mod style;
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
pub const MUTE_KEY: char = 'm';

const NUM_TABS: usize = 3;
const TAB_NAMES: [&str; NUM_TABS] = [TAB_NAME_PEERS, TAB_NAME_CHAT, TAB_NAME_SETTINGS];

const EVENT_BATCH_BOUND: usize = 64;
const LOUDNESS_RENDER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerState {
    Connected(String),
    Disconnected,
    Disabled,
    Connecting(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    id: String,
    display_name: Option<String>,
    state: PeerState,
    denoised: DenoiseSelection,
    volume: usize,
    loudness: f64,
}

impl Peer {
    pub fn new(
        id: String,
        display_name: Option<String>,
        state: PeerState,
        denoised: DenoiseSelection,
        volume: usize,
    ) -> Peer {
        Peer {
            id,
            display_name,
            state,
            denoised,
            volume,
            loudness: 0.0,
        }
    }

    pub fn with_denoised(self, denoised: DenoiseSelection) -> Peer {
        Peer { denoised, ..self }
    }

    pub fn with_state(self, state: PeerState) -> Peer {
        Peer { state, ..self }
    }

    pub fn with_volume(self, volume: usize) -> Peer {
        Peer { volume, ..self }
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
    Delete,
    Left,
    Right,
    CursorBeginning,
    CursorEnd,
    PreviousWord,
    NextWord,
    DeleteWord,
    SetOwnPublicKey(String),
    SetOwnDisplayName(String),
    SetServer(Vec<String>),
    SetRoom(String),
    SetRoomFingerprint(String),
    Down,
    Up,
    TogglePeer,
    ToggleDenoise,
    SetPeerDenoise(String, DenoiseSelection),
    SetPeerVolume(String, usize),
    MuteSelf(bool),
    Loudness(String, f64),
    SetInputDeviceName(String),
    SetOutputDeviceName(String),
}

impl AppEvent {
    fn is_low_priority(&self) -> bool {
        matches!(self, AppEvent::Loudness(..) | AppEvent::AddPeer(_))
    }
}

pub struct App {
    pub user_action_sender: UnboundedSender<UserInputEvent>,
    pub tabs: [&'static str; NUM_TABS],
    pub tab_index: usize,
    pub killed: bool,
    pub peers: BTreeMap<String, Peer>, // (Onion Address, Peer)
    pub own_public_key: Option<String>,
    pub own_display_name: Option<String>,
    pub servers: Vec<String>,
    pub room: Option<String>,
    pub room_fingerprint: Option<String>,
    pub editor: Editor,
    pub peer_index: usize,
    pub chat_history: Vec<(String, String)>, // (Display Name, Message)
    pub unread_messages: bool,
    pub chat_offset: usize, // Offset from bottom of chat in full messages.
    pub mute_self: bool,
    pub input_device_name: String,
    pub output_device_name: String,
}

impl App {
    pub fn new(sender: UnboundedSender<UserInputEvent>) -> App {
        App {
            user_action_sender: sender,
            tabs: TAB_NAMES,
            tab_index: 0,
            killed: false,
            peers: BTreeMap::new(),
            own_public_key: None,
            own_display_name: None,
            servers: vec![],
            room: None,
            room_fingerprint: None,
            editor: Editor::new(),
            peer_index: 0,
            chat_history: vec![],
            unread_messages: false,
            chat_offset: 0,
            mute_self: false,
            input_device_name: "".into(),
            output_device_name: "".into(),
        }
    }

    /// Applies an event
    /// Returns whether anything visibly changed.
    fn process_event(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::Kill => {
                self.killed = true;
                true
            }
            AppEvent::Nothing => true,
            AppEvent::NextTab => {
                self.move_tabs(1);
                true
            }
            AppEvent::PreviousTab => {
                self.move_tabs(-1);
                true
            }
            AppEvent::AddPeer(mut peer) => {
                if let Some(existing) = self.peers.get(&peer.id) {
                    peer.loudness = existing.loudness;
                }
                let changed = self.peers.get(&peer.id) != Some(&peer);
                if changed {
                    self.peers.insert(peer.id.clone(), peer);
                }
                changed
            }
            AppEvent::RemovePeer(id) => self.peers.remove(&id).is_some(),
            AppEvent::Character(c) => match self.tab_index {
                TAB_IDX_PEERS => match c {
                    TOGGLE_PEER_KEY => {
                        self.toggle_peer();
                        true
                    }
                    TOGGLE_PEER_DENOISE_KEY => {
                        self.toggle_denoise();
                        true
                    }
                    INCREMENT_PEER_VOLUME_KEY => {
                        self.adjust_volume(1);
                        true
                    }
                    DECREMENT_PEER_VOLUME_KEY => {
                        self.adjust_volume(-1);
                        true
                    }
                    MOVE_DOWN_PEER_LIST_KEY => self.move_peer(1),
                    MOVE_UP_PEER_LIST_KEY => self.move_peer(-1),
                    MOVE_TOP_PEER_LIST_KEY => {
                        let old = self.peer_index;
                        self.peer_index = 0;
                        old != self.peer_index
                    }
                    MOVE_BOTTOM_PEER_LIST_KEY => {
                        if self.peers.is_empty() {
                            false
                        } else {
                            let old = self.peer_index;
                            self.peer_index = self.peers.len() - 1;
                            old != self.peer_index
                        }
                    }
                    MUTE_KEY => {
                        self.toggle_mute_self();
                        true
                    }
                    _ => false,
                },
                TAB_IDX_CHAT => {
                    self.editor.append(c);
                    true
                }
                _ => false,
            },
            AppEvent::Enter => {
                let effective = self.tab_index == TAB_IDX_CHAT && !self.editor.is_empty();
                if effective {
                    self.send_message();
                }
                effective
            }
            AppEvent::NewMessage(sender_name, message) => {
                self.add_message((sender_name, message));
                if self.tab_index != TAB_IDX_CHAT || self.chat_offset > 0 {
                    self.unread_messages = true;
                }
                true
            }
            AppEvent::Backspace => {
                let old = self.editor.cursor;
                self.editor.backspace();
                old != self.editor.cursor
            }
            AppEvent::Delete => {
                let old = self.editor.buffer.clone();
                self.editor.delete();
                old != self.editor.buffer
            }
            AppEvent::Left => {
                let old = self.editor.cursor;
                self.editor.left();
                old != self.editor.cursor
            }
            AppEvent::Right => {
                let old = self.editor.cursor;
                self.editor.right();
                old != self.editor.cursor
            }
            AppEvent::CursorBeginning => {
                let old = self.editor.cursor;
                self.editor.cursor_beginning();
                old != self.editor.cursor
            }
            AppEvent::CursorEnd => {
                let old = self.editor.cursor;
                self.editor.cursor_end();
                old != self.editor.cursor
            }
            AppEvent::PreviousWord => {
                let old = self.editor.cursor;
                self.editor.previous_word();
                old != self.editor.cursor
            }
            AppEvent::NextWord => {
                let old = self.editor.cursor;
                self.editor.next_word();
                old != self.editor.cursor
            }
            AppEvent::DeleteWord => {
                let old = self.editor.cursor;
                self.editor.delete_word();
                old != self.editor.cursor
            }
            AppEvent::SetOwnPublicKey(address) => {
                self.own_public_key = Some(address);
                true
            }
            AppEvent::SetOwnDisplayName(display_name) => {
                self.own_display_name = Some(display_name);
                true
            }
            AppEvent::SetServer(server) => {
                self.servers = server;
                true
            }
            AppEvent::SetRoom(room) => {
                self.room = Some(room);
                true
            }
            AppEvent::SetRoomFingerprint(room_fingerprint) => {
                self.room_fingerprint = Some(room_fingerprint);
                true
            }
            AppEvent::Down => match self.tab_index {
                TAB_IDX_PEERS => {
                    if self.peers.is_empty() {
                        false
                    } else {
                        let old = self.peer_index;
                        self.peer_index = std::cmp::min(
                            self.peer_index.checked_add(1).unwrap_or(0),
                            self.peers.len() - 1,
                        );
                        old != self.peer_index
                    }
                }
                TAB_IDX_CHAT => {
                    let old = self.chat_offset;
                    self.chat_offset = self.chat_offset.saturating_sub(1);
                    if self.chat_offset == 0 {
                        self.unread_messages = false;
                    }
                    old != self.chat_offset
                }
                _ => false,
            },
            AppEvent::Up => match self.tab_index {
                TAB_IDX_PEERS => {
                    if self.peers.is_empty() {
                        false
                    } else {
                        let old = self.peer_index;
                        self.peer_index = self.peer_index.saturating_sub(1);
                        old != self.peer_index
                    }
                }
                TAB_IDX_CHAT => {
                    let old = self.chat_offset;
                    self.chat_offset = std::cmp::min(self.chat_history.len(), self.chat_offset + 1);
                    old != self.chat_offset
                }
                _ => false,
            },
            AppEvent::TogglePeer => {
                if self.selected_peer().is_none() {
                    false
                } else {
                    self.toggle_peer();
                    true
                }
            }
            AppEvent::ToggleDenoise => {
                if self.selected_peer().is_none() {
                    false
                } else {
                    self.toggle_denoise();
                    true
                }
            }
            AppEvent::SetPeerDenoise(peer_id, denoised) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    let changed = peer.denoised != denoised;
                    peer.denoised = denoised;
                    changed
                } else {
                    false
                }
            }
            AppEvent::SetPeerVolume(peer_id, volume) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    let changed = peer.volume != volume;
                    peer.volume = volume;
                    changed
                } else {
                    false
                }
            }
            AppEvent::MuteSelf(is_muted) => {
                let changed = self.mute_self != is_muted;
                self.mute_self = is_muted;
                changed
            }
            AppEvent::SetInputDeviceName(input_device_name) => {
                self.input_device_name = input_device_name;
                true
            }
            AppEvent::SetOutputDeviceName(output_device_name) => {
                self.output_device_name = output_device_name;
                true
            }
            AppEvent::Loudness(peer_id, level) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    let name = peer.display_name.as_ref().unwrap_or(&peer.id).clone();
                    let changed = render::loudness_bucket(&name, peer.loudness)
                        != render::loudness_bucket(&name, level);
                    if changed {
                        peer.loudness = level;
                    }
                    changed
                } else {
                    false
                }
            }
        }
    }

    fn move_peer(&mut self, delta: isize) -> bool {
        if self.peers.is_empty() {
            false
        } else {
            let old = self.peer_index;
            self.peer_index = add_in_bounds(self.peer_index, 0, self.peers.len() - 1, delta);
            old != self.peer_index
        }
    }

    fn selected_peer(&self) -> Option<&Peer> {
        self.peers.values().nth(self.peer_index)
    }

    fn toggle_peer(&mut self) {
        if let Some(peer) = self.selected_peer() {
            if peer.state == PeerState::Disabled {
                self.user_action_sender
                    .send(UserInputEvent::EnablePeer(peer.id.clone()))
                    .unwrap();
            } else {
                self.user_action_sender
                    .send(UserInputEvent::DisablePeer(peer.id.clone()))
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
            let own_address = self.own_public_key.clone().unwrap_or(default);
            self.add_message((own_address, message.clone()));
            self.user_action_sender
                .send(UserInputEvent::SendMessage(message))
                .unwrap();
        }
    }

    fn toggle_denoise(&mut self) {
        if let Some(peer) = self.selected_peer() {
            self.user_action_sender
                .send(UserInputEvent::SetDenoise(
                    peer.id.clone(),
                    peer.denoised.next(),
                ))
                .unwrap();
        }
    }

    fn adjust_volume(&mut self, delta: isize) {
        if let Some(peer) = self.selected_peer() {
            self.user_action_sender
                .send(UserInputEvent::SetVolume(
                    peer.id.clone(),
                    add_in_bounds(peer.volume, 0, 999, delta),
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

    fn toggle_mute_self(&mut self) {
        self.mute_self = !self.mute_self;
        self.user_action_sender
            .send(UserInputEvent::SetMuteSelf(self.mute_self))
            .unwrap();
    }

    pub fn render<B: Backend>(&self, terminal: &mut Terminal<B>) -> io::Result<bool> {
        terminal.draw(|f| render::ui(f, self)).unwrap();
        Ok(self.killed)
    }
}

pub async fn get_sender(
    mut app: App,
    mut terminal: DefaultTerminal,
) -> (UnboundedSender<AppEvent>, JoinHandle<DefaultTerminal>) {
    let (sender, mut receiver): (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) =
        unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut batch = Vec::with_capacity(EVENT_BATCH_BOUND);
        let mut pending: Vec<AppEvent> = Vec::new();
        let mut ticker = tokio::time::interval(LOUDNESS_RENDER_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                biased;
                count = receiver.recv_many(&mut batch, EVENT_BATCH_BOUND) => {
                    if count == 0 {
                        break;
                    }
                    if drain_batch(&mut app, &mut batch, &mut pending)
                        && let Ok(true) = app.render(&mut terminal)
                    {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if flush_pending(&mut app, &mut pending)
                        && let Ok(true) = app.render(&mut terminal)
                    {
                        break;
                    }
                }
            }
        }
        terminal
    });
    sender.send(AppEvent::Nothing).unwrap();
    (sender, handle)
}

fn drain_batch(app: &mut App, batch: &mut Vec<AppEvent>, pending: &mut Vec<AppEvent>) -> bool {
    let mut render = false;
    for event in batch.drain(..) {
        if event.is_low_priority() {
            pending.push(event);
        } else {
            // Preserve arrival order: deferred events apply first
            render |= flush_pending(app, pending);
            render |= app.process_event(event);
        }
    }
    render
}

fn flush_pending(app: &mut App, pending: &mut Vec<AppEvent>) -> bool {
    let mut dirty = false;
    for event in pending.drain(..) {
        dirty |= app.process_event(event);
    }
    dirty
}

pub async fn handle_input(sender: UnboundedSender<AppEvent>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        loop {
            match event::read().unwrap() {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
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
                            KeyCode::Delete => {
                                sender.send(AppEvent::Delete).unwrap();
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
        }
    })
}

pub async fn start_tui() -> Result<
    (
        UnboundedSender<AppEvent>,
        UnboundedReceiver<UserInputEvent>,
        JoinHandle<DefaultTerminal>,
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

pub async fn stop_tui(handle: JoinHandle<DefaultTerminal>) -> Result<(), Box<dyn Error>> {
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

#[cfg(test)]
mod render_scaling_tests {
    use super::*;
    use insanity_core::user_input_event::DenoiseSelection;
    use ratatui::{Terminal, backend::TestBackend};

    const TICKS: usize = 100;
    const HI_PRI_AT: [usize; 5] = [10, 30, 50, 70, 90];

    fn test_terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(80, 24)).expect("test terminal")
    }

    fn test_app_with_peers(count: usize) -> (App, Vec<String>) {
        let (tx, _rx) = unbounded_channel();
        let mut app = App::new(tx);
        let mut ids = Vec::new();
        for i in 0..count {
            let id = format!("peer-{i:02}");
            app.peers.insert(
                id.clone(),
                Peer::new(
                    id.clone(),
                    Some(format!("peer-{i:02}")),
                    PeerState::Connected("addr".to_string()),
                    DenoiseSelection::None,
                    100,
                ),
            );
            ids.push(id);
        }
        (app, ids)
    }

    fn level_for(tick: usize, peer: usize) -> f64 {
        0.5 + 0.4 * ((tick as f64 / 10.0) + peer as f64).sin()
    }

    fn run_policy(
        app: &mut App,
        terminal: &mut Terminal<TestBackend>,
        peer_ids: &[String],
    ) -> usize {
        let mut draws = 0;
        let mut pending: Vec<AppEvent> = Vec::new();
        let mut batch = Vec::new();
        for tick in 0..TICKS {
            batch.clear();
            for (p, id) in peer_ids.iter().enumerate() {
                batch.push(AppEvent::Loudness(id.clone(), level_for(tick, p)));
            }
            if HI_PRI_AT.contains(&tick) {
                batch.push(AppEvent::Nothing);
            }
            if drain_batch(app, &mut batch, &mut pending) {
                app.render(terminal).expect("render");
                draws += 1;
            }
            if flush_pending(app, &mut pending) {
                app.render(terminal).expect("render");
                draws += 1;
            }
        }
        draws
    }

    #[test]
    fn draws_scale_with_ticks_not_peers() {
        for count in [0, 2, 6, 10] {
            let (mut app, ids) = test_app_with_peers(count);
            let mut terminal = test_terminal();
            let draws = run_policy(&mut app, &mut terminal, &ids);
            assert!(
                draws <= TICKS + HI_PRI_AT.len(),
                "N={count}: {draws} draws exceed tick + hi-pri budget"
            );
            let old_behavior = count * TICKS + HI_PRI_AT.len();
            if count > 0 {
                assert!(
                    draws < old_behavior,
                    "N={count}: {draws} draws show no coalescing vs {old_behavior} before"
                );
            }
        }
    }

    #[test]
    fn silent_room_draws_only_for_hi_pri() {
        let (mut app, ids) = test_app_with_peers(0);
        assert!(ids.is_empty());
        let mut terminal = test_terminal();
        let draws = run_policy(&mut app, &mut terminal, &ids);
        assert_eq!(draws, HI_PRI_AT.len());
    }

    #[test]
    fn same_bucket_burst_draws_once() {
        let (mut app, ids) = test_app_with_peers(1);
        let mut pending: Vec<AppEvent> = Vec::new();
        for _ in 0..50 {
            pending.push(AppEvent::Loudness(ids[0].clone(), 0.5));
        }
        let mut terminal = test_terminal();
        let mut draws = 0;
        if flush_pending(&mut app, &mut pending) {
            app.render(&mut terminal).expect("render");
            draws += 1;
        }
        assert_eq!(draws, 1);
        assert!(pending.is_empty());
        assert!((app.peers[&ids[0]].loudness - 0.5).abs() < f64::EPSILON);
        assert!(!flush_pending(
            &mut app,
            &mut vec![AppEvent::Loudness(ids[0].clone(), 0.5)]
        ));
    }

    #[test]
    fn forward_delete_removes_char_under_cursor() {
        let (tx, _rx) = unbounded_channel();
        let mut app = App::new(tx);
        assert!(app.process_event(AppEvent::NextTab));
        assert!(app.process_event(AppEvent::Character('h')));
        assert!(app.process_event(AppEvent::Character('i')));
        assert!(app.process_event(AppEvent::Left));
        assert!(app.process_event(AppEvent::Delete));
        assert_eq!(app.editor.buffer, "h");
        assert_eq!(app.editor.cursor, 1);
        assert!(!app.process_event(AppEvent::Delete));
        assert_eq!(app.editor.buffer, "h");
        assert!(app.process_event(AppEvent::Backspace));
        assert_eq!(app.editor.buffer, "");
    }

    #[test]
    fn hi_pri_applies_pending_before_processing() {
        let (mut app, ids) = test_app_with_peers(2);
        let mut pending: Vec<AppEvent> = Vec::new();
        let mut batch = vec![
            AppEvent::Loudness(ids[0].clone(), 0.5),
            AppEvent::Loudness(ids[0].clone(), 0.9),
            AppEvent::Nothing,
        ];
        assert!(drain_batch(&mut app, &mut batch, &mut pending));
        assert!(
            pending.is_empty(),
            "sequential rule: pending applied inline before the hi-pri event"
        );
        assert!(
            (app.peers[&ids[0]].loudness - 0.9).abs() < f64::EPSILON,
            "latest queued level wins in arrival order"
        );
        let mut terminal = test_terminal();
        app.render(&mut terminal).expect("render");
    }

    #[test]
    fn unknown_peer_loudness_is_dropped_safely() {
        let (mut app, _) = test_app_with_peers(1);
        let mut pending = vec![AppEvent::Loudness("ghost".to_string(), 0.7)];
        let mut terminal = test_terminal();
        assert!(!flush_pending(&mut app, &mut pending));
        app.render(&mut terminal).expect("render");
        assert!(!app.peers.contains_key("ghost"));
        assert!(pending.is_empty());
    }

    #[test]
    fn process_event_reports_effectiveness() {
        let (mut app, ids) = test_app_with_peers(1);
        assert!(app.process_event(AppEvent::Loudness(ids[0].clone(), 0.9)));
        assert!(!app.process_event(AppEvent::Loudness(ids[0].clone(), 0.9)));
        assert!(!app.process_event(AppEvent::Loudness("ghost".to_string(), 0.9)));
        assert!(app.process_event(AppEvent::Nothing));
        assert!(!app.process_event(AppEvent::RemovePeer("ghost".to_string())));
        assert!(app.process_event(AppEvent::RemovePeer(ids[0].clone())));
        assert!(!app.process_event(AppEvent::SetPeerVolume(ids[0].clone(), 100)));
        assert!(!app.process_event(AppEvent::MuteSelf(false)));
        assert!(app.process_event(AppEvent::MuteSelf(true)));
        assert!(!app.process_event(AppEvent::Enter));
        assert!(!app.process_event(AppEvent::Character('x')));
        assert!(!app.process_event(AppEvent::Up));
        let (mut app, _) = test_app_with_peers(2);
        assert!(app.process_event(AppEvent::Down));
        assert!(!app.process_event(AppEvent::Down));
        assert!(app.process_event(AppEvent::Up));
    }

    #[test]
    fn identical_connecting_updates_draw_nothing() {
        let (mut app, _) = test_app_with_peers(0);
        let mut terminal = test_terminal();
        let update = || {
            AppEvent::AddPeer(Peer::new(
                "peer-00".to_string(),
                Some("peer-00".to_string()),
                PeerState::Connecting("addr".to_string()),
                DenoiseSelection::None,
                100,
            ))
        };
        let mut draws = 0;
        let mut pending: Vec<AppEvent> = Vec::new();
        for _ in 0..20 {
            let mut batch = vec![update()];
            if drain_batch(&mut app, &mut batch, &mut pending) {
                app.render(&mut terminal).expect("render");
                draws += 1;
            }
            if flush_pending(&mut app, &mut pending) {
                app.render(&mut terminal).expect("render");
                draws += 1;
            }
        }
        assert_eq!(draws, 1, "only the first insert draws");
    }

    #[test]
    fn reannounce_preserves_live_loudness() {
        let (mut app, ids) = test_app_with_peers(1);
        assert!(app.process_event(AppEvent::Loudness(ids[0].clone(), 0.9)));
        let live = Peer::new(
            ids[0].clone(),
            Some(ids[0].clone()),
            PeerState::Connected("addr".to_string()),
            DenoiseSelection::None,
            100,
        );
        assert!(
            !app.process_event(AppEvent::AddPeer(live)),
            "re-announce with identical roster must not draw or clobber"
        );
        assert!(
            (app.peers[&ids[0]].loudness - 0.9).abs() < f64::EPSILON,
            "live meter level survives the re-announce"
        );
    }

    #[test]
    fn zero_peer_movement_draws_nothing() {
        let (mut app, _) = test_app_with_peers(0);
        assert!(!app.process_event(AppEvent::Down));
        assert!(!app.process_event(AppEvent::Up));
        assert!(!app.process_event(AppEvent::Character('G')));
        assert!(!app.process_event(AppEvent::Character('j')));
    }

    #[test]
    fn burst_traffic_absorbed_within_tick_budget() {
        let (mut app, ids) = test_app_with_peers(10);
        let mut terminal = test_terminal();
        let mut pending: Vec<AppEvent> = Vec::new();
        let mut draws = 0;
        for tick in 0..TICKS {
            let mut batch = Vec::new();
            if tick == 50 {
                for (p, id) in ids.iter().enumerate() {
                    for _ in 0..10 {
                        batch.push(AppEvent::Loudness(id.clone(), level_for(tick, p)));
                    }
                }
            }
            if drain_batch(&mut app, &mut batch, &mut pending) {
                app.render(&mut terminal).expect("render");
                draws += 1;
            }
            if flush_pending(&mut app, &mut pending) {
                app.render(&mut terminal).expect("render");
                draws += 1;
            }
        }
        assert_eq!(
            pending.len(),
            0,
            "coalescing bounds the queue even under burst traffic"
        );
        assert!(
            draws <= 2,
            "one burst tick draws at most once for flush plus nothing elsewhere, got {draws}"
        );
    }

    fn expected_low_priority(event: &AppEvent) -> bool {
        match event {
            AppEvent::Loudness(..) | AppEvent::AddPeer(_) => true,
            AppEvent::Kill
            | AppEvent::NextTab
            | AppEvent::PreviousTab
            | AppEvent::Nothing
            | AppEvent::Character(_)
            | AppEvent::Enter
            | AppEvent::NewMessage(_, _)
            | AppEvent::RemovePeer(_)
            | AppEvent::Backspace
            | AppEvent::Delete
            | AppEvent::Left
            | AppEvent::Right
            | AppEvent::CursorBeginning
            | AppEvent::CursorEnd
            | AppEvent::PreviousWord
            | AppEvent::NextWord
            | AppEvent::DeleteWord
            | AppEvent::SetOwnPublicKey(_)
            | AppEvent::SetOwnDisplayName(_)
            | AppEvent::SetServer(_)
            | AppEvent::SetRoom(_)
            | AppEvent::SetRoomFingerprint(_)
            | AppEvent::Down
            | AppEvent::Up
            | AppEvent::TogglePeer
            | AppEvent::ToggleDenoise
            | AppEvent::SetPeerDenoise(_, _)
            | AppEvent::SetPeerVolume(_, _)
            | AppEvent::MuteSelf(_)
            | AppEvent::SetInputDeviceName(_)
            | AppEvent::SetOutputDeviceName(_) => false,
        }
    }

    #[test]
    fn classifier_covers_every_variant() {
        let peer = Peer::new(
            "id".to_string(),
            None,
            PeerState::Disconnected,
            DenoiseSelection::None,
            100,
        );
        let events = vec![
            AppEvent::Kill,
            AppEvent::NextTab,
            AppEvent::PreviousTab,
            AppEvent::Nothing,
            AppEvent::Character('x'),
            AppEvent::Enter,
            AppEvent::NewMessage("a".to_string(), "b".to_string()),
            AppEvent::AddPeer(peer),
            AppEvent::RemovePeer("id".to_string()),
            AppEvent::Backspace,
            AppEvent::Delete,
            AppEvent::Left,
            AppEvent::Right,
            AppEvent::CursorBeginning,
            AppEvent::CursorEnd,
            AppEvent::PreviousWord,
            AppEvent::NextWord,
            AppEvent::DeleteWord,
            AppEvent::SetOwnPublicKey("k".to_string()),
            AppEvent::SetOwnDisplayName("n".to_string()),
            AppEvent::SetServer(vec![]),
            AppEvent::SetRoom("r".to_string()),
            AppEvent::SetRoomFingerprint("f".to_string()),
            AppEvent::Down,
            AppEvent::Up,
            AppEvent::TogglePeer,
            AppEvent::ToggleDenoise,
            AppEvent::SetPeerDenoise("id".to_string(), DenoiseSelection::None),
            AppEvent::SetPeerVolume("id".to_string(), 1),
            AppEvent::MuteSelf(false),
            AppEvent::Loudness("id".to_string(), 0.5),
            AppEvent::SetInputDeviceName("i".to_string()),
            AppEvent::SetOutputDeviceName("o".to_string()),
        ];
        for event in &events {
            assert_eq!(
                event.is_low_priority(),
                expected_low_priority(event),
                "classifier disagrees on {event:?}"
            );
        }
    }
}
