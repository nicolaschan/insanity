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

const TAB_NAME_MAIN: &str = "Main";
const TAB_NAME_CHAT: &str = "Chat";
const TAB_NAME_SETTINGS: &str = "Settings";

// Order must match in TAB_NAMES.
pub(crate) const TAB_IDX_MAIN: usize = 0;
pub(crate) const TAB_IDX_CHAT: usize = 1;
pub(crate) const TAB_IDX_SETTINGS: usize = 2;

pub(crate) const TOGGLE_PEER_KEY: char = ' ';
pub(crate) const TOGGLE_PEER_DENOISE_KEY: char = 'd';
pub(crate) const INCREMENT_PEER_VOLUME_KEY: char = '+';
pub(crate) const DECREMENT_PEER_VOLUME_KEY: char = '-';
pub(crate) const MOVE_DOWN_PEER_LIST_KEY: char = 'j';
pub(crate) const MOVE_UP_PEER_LIST_KEY: char = 'k';
pub(crate) const MOVE_TOP_PEER_LIST_KEY: char = 'g';
pub(crate) const MOVE_BOTTOM_PEER_LIST_KEY: char = 'G';
pub(crate) const MUTE_KEY: char = 'm';
pub(crate) const REFRESH_DEVICES_KEY: char = 'r';

const NUM_TABS: usize = 3;
const TAB_NAMES: [&str; NUM_TABS] = [TAB_NAME_MAIN, TAB_NAME_CHAT, TAB_NAME_SETTINGS];

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
    SetSpeakerVolume(usize),
    SetMicVolume(usize),
    MuteSelf(bool),
    Loudness(String, f64),
    SetInputDeviceName(String),
    SetOutputDeviceName(String),
    SetInputDevices(Vec<(String, String)>),
    SetOutputDevices(Vec<(String, String)>),
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
    pub main_cursor: MainCursor,
    pub speaker_volume: usize,
    pub mic_volume: usize,
    pub chat_history: Vec<(String, String)>, // (Display Name, Message)
    pub unread_messages: bool,
    pub chat_offset: usize, // Offset from bottom of chat in full messages.
    pub mute_self: bool,
    pub input_device_name: String,
    pub output_device_name: String,
    pub input_devices: Vec<(String, String)>, // (Device ID, Device Name)
    pub output_devices: Vec<(String, String)>, // (Device ID, Device Name)
    pub device_cursor: DeviceCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainCursor {
    Speaker,
    Mic,
    Peer(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceCursor {
    Input(usize),
    Output(usize),
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
            main_cursor: MainCursor::Speaker,
            speaker_volume: 100,
            mic_volume: 100,
            chat_history: vec![],
            unread_messages: false,
            chat_offset: 0,
            mute_self: false,
            input_device_name: "".into(),
            output_device_name: "".into(),
            input_devices: vec![],
            output_devices: vec![],
            device_cursor: DeviceCursor::Input(0),
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
            AppEvent::RemovePeer(id) => {
                let removed = self.peers.remove(&id).is_some();
                self.clamp_main_cursor();
                removed
            }
            AppEvent::Character(c) => match self.tab_index {
                TAB_IDX_MAIN => match c {
                    TOGGLE_PEER_KEY => self.toggle_peer(),
                    TOGGLE_PEER_DENOISE_KEY => self.toggle_denoise(),
                    INCREMENT_PEER_VOLUME_KEY => self.adjust_volume(1),
                    DECREMENT_PEER_VOLUME_KEY => self.adjust_volume(-1),
                    MOVE_DOWN_PEER_LIST_KEY => self.move_main(1),
                    MOVE_UP_PEER_LIST_KEY => self.move_main(-1),
                    MOVE_TOP_PEER_LIST_KEY => {
                        let old = self.main_cursor;
                        self.main_cursor = MainCursor::Speaker;
                        old != self.main_cursor
                    }
                    MOVE_BOTTOM_PEER_LIST_KEY => {
                        if self.peers.is_empty() {
                            let old = self.main_cursor;
                            self.main_cursor = MainCursor::Mic;
                            old != self.main_cursor
                        } else {
                            let old = self.main_cursor;
                            self.main_cursor = MainCursor::Peer(self.peers.len() - 1);
                            old != self.main_cursor
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
                TAB_IDX_SETTINGS => match c {
                    MOVE_DOWN_PEER_LIST_KEY => self.move_device_cursor(1),
                    MOVE_UP_PEER_LIST_KEY => self.move_device_cursor(-1),
                    REFRESH_DEVICES_KEY => {
                        self.refresh_devices();
                        true
                    }
                    _ => false,
                },
                _ => false,
            },
            AppEvent::Enter => {
                if self.tab_index == TAB_IDX_SETTINGS {
                    self.select_focused_device()
                } else if self.tab_index == TAB_IDX_CHAT {
                    if !self.editor.is_empty() {
                        self.send_message();
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
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
                TAB_IDX_MAIN => self.move_main(1),
                TAB_IDX_CHAT => {
                    let old = self.chat_offset;
                    self.chat_offset = self.chat_offset.saturating_sub(1);
                    if self.chat_offset == 0 {
                        self.unread_messages = false;
                    }
                    old != self.chat_offset
                }
                TAB_IDX_SETTINGS => self.move_device_cursor(1),
                _ => false,
            },
            AppEvent::Up => match self.tab_index {
                TAB_IDX_MAIN => self.move_main(-1),
                TAB_IDX_CHAT => {
                    let old = self.chat_offset;
                    self.chat_offset = std::cmp::min(self.chat_history.len(), self.chat_offset + 1);
                    old != self.chat_offset
                }
                TAB_IDX_SETTINGS => self.move_device_cursor(-1),
                _ => false,
            },
            AppEvent::TogglePeer => self.toggle_peer(),
            AppEvent::ToggleDenoise => self.toggle_denoise(),
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
            AppEvent::SetSpeakerVolume(volume) => {
                let changed = self.speaker_volume != volume;
                self.speaker_volume = volume;
                changed
            }
            AppEvent::SetMicVolume(volume) => {
                let changed = self.mic_volume != volume;
                self.mic_volume = volume;
                changed
            }
            AppEvent::MuteSelf(is_muted) => {
                let changed = self.mute_self != is_muted;
                self.mute_self = is_muted;
                changed
            }
            AppEvent::SetInputDeviceName(input_device_name) => {
                let changed = self.input_device_name != input_device_name;
                self.input_device_name = input_device_name;
                changed
            }
            AppEvent::SetOutputDeviceName(output_device_name) => {
                let changed = self.output_device_name != output_device_name;
                self.output_device_name = output_device_name;
                changed
            }
            AppEvent::SetInputDevices(devices) => {
                let changed = self.input_devices != devices;
                self.input_devices = devices;
                self.clamp_device_cursor() || changed
            }
            AppEvent::SetOutputDevices(devices) => {
                let changed = self.output_devices != devices;
                self.output_devices = devices;
                self.clamp_device_cursor() || changed
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

    /// Position of `cursor` in the seamless Main-tab chain
    /// Speaker(0) -> Mic(1) -> Peer(0..n).
    fn main_position(cursor: MainCursor) -> usize {
        match cursor {
            MainCursor::Speaker => 0,
            MainCursor::Mic => 1,
            MainCursor::Peer(i) => 2 + i,
        }
    }

    /// Total rows in the Main-tab chain (2 volume rows + peers).
    fn main_len(&self) -> usize {
        2 + self.peers.len()
    }

    fn cursor_at_position(&self, pos: usize) -> MainCursor {
        if pos == 0 {
            MainCursor::Speaker
        } else if pos == 1 {
            MainCursor::Mic
        } else {
            MainCursor::Peer(pos - 2)
        }
    }

    fn move_main(&mut self, delta: isize) -> bool {
        let len = self.main_len();
        debug_assert!(len >= 2);
        let old = self.main_cursor;
        let pos = Self::main_position(old);
        let new_pos = add_in_bounds(pos, 0, len - 1, delta);
        self.main_cursor = self.cursor_at_position(new_pos);
        old != self.main_cursor
    }

    /// Re-park the Main cursor after peer list changes. Returns whether it moved.
    fn clamp_main_cursor(&mut self) -> bool {
        let old = self.main_cursor;
        let len = self.main_len();
        let pos = Self::main_position(old);
        let clamped = std::cmp::min(pos, len - 1);
        self.main_cursor = self.cursor_at_position(clamped);
        old != self.main_cursor
    }

    fn selected_peer(&self) -> Option<&Peer> {
        match self.main_cursor {
            MainCursor::Peer(i) => self.peers.values().nth(i),
            _ => None,
        }
    }

    fn clamp_device_cursor(&mut self) -> bool {
        let clamp = |row: usize, len: usize| std::cmp::min(row, len.saturating_sub(1));
        let clamped = match (
            self.device_cursor,
            self.input_devices.is_empty(),
            self.output_devices.is_empty(),
        ) {
            (_, true, true) => DeviceCursor::Input(0),
            (DeviceCursor::Input(row), false, _) => {
                DeviceCursor::Input(clamp(row, self.input_devices.len()))
            }
            (DeviceCursor::Output(row), _, false) => {
                DeviceCursor::Output(clamp(row, self.output_devices.len()))
            }
            (DeviceCursor::Input(_), true, false) => DeviceCursor::Output(0),
            (DeviceCursor::Output(_), false, true) => {
                DeviceCursor::Input(self.input_devices.len() - 1)
            }
        };
        let changed = clamped != self.device_cursor;
        self.device_cursor = clamped;
        changed
    }

    fn move_device_cursor(&mut self, delta: isize) -> bool {
        if self.input_devices.is_empty() && self.output_devices.is_empty() {
            return false;
        }
        let old = self.device_cursor;
        self.device_cursor = match (self.device_cursor, delta.signum()) {
            (DeviceCursor::Input(row), 1) => {
                if row + 1 < self.input_devices.len() {
                    DeviceCursor::Input(row + 1)
                } else if self.output_devices.is_empty() {
                    DeviceCursor::Input(row)
                } else {
                    DeviceCursor::Output(0)
                }
            }
            (DeviceCursor::Output(row), 1) => {
                if row + 1 < self.output_devices.len() {
                    DeviceCursor::Output(row + 1)
                } else {
                    DeviceCursor::Output(row)
                }
            }
            (DeviceCursor::Input(row), _) => {
                if row > 0 && !self.input_devices.is_empty() {
                    DeviceCursor::Input(row - 1)
                } else {
                    DeviceCursor::Input(0)
                }
            }
            (DeviceCursor::Output(row), _) => {
                if row > 0 && !self.output_devices.is_empty() {
                    DeviceCursor::Output(row - 1)
                } else if self.input_devices.is_empty() {
                    DeviceCursor::Output(0)
                } else {
                    DeviceCursor::Input(self.input_devices.len() - 1)
                }
            }
        };
        old != self.device_cursor
    }

    fn select_focused_device(&mut self) -> bool {
        let selected = match self.device_cursor {
            DeviceCursor::Input(row) => self
                .input_devices
                .get(row)
                .map(|(id, name)| UserInputEvent::SetInputDevice(id.clone(), name.clone())),
            DeviceCursor::Output(row) => self
                .output_devices
                .get(row)
                .map(|(id, name)| UserInputEvent::SetOutputDevice(id.clone(), name.clone())),
        };
        match selected {
            Some(event) => self.user_action_sender.send(event).is_ok(),
            None => false,
        }
    }

    fn refresh_devices(&mut self) {
        self.user_action_sender
            .send(UserInputEvent::RefreshDevices)
            .unwrap();
    }

    fn toggle_peer(&mut self) -> bool {
        if let Some(peer) = self.selected_peer() {
            if peer.state == PeerState::Disabled {
                self.user_action_sender
                    .send(UserInputEvent::EnablePeer(peer.id.clone()))
                    .is_ok()
            } else {
                self.user_action_sender
                    .send(UserInputEvent::DisablePeer(peer.id.clone()))
                    .is_ok()
            }
        } else {
            false
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

    fn toggle_denoise(&mut self) -> bool {
        if let Some(peer) = self.selected_peer() {
            self.user_action_sender
                .send(UserInputEvent::SetDenoise(
                    peer.id.clone(),
                    peer.denoised.next(),
                ))
                .is_ok()
        } else {
            false
        }
    }

    fn adjust_volume(&mut self, delta: isize) -> bool {
        // The volumes here are clamped to 0, 999 because
        // we've restricted 3 characters for volume display.
        match self.main_cursor {
            MainCursor::Speaker => {
                let new_volume = add_in_bounds(self.speaker_volume, 0, 999, delta);
                if new_volume == self.speaker_volume {
                    false
                } else {
                    self.user_action_sender
                        .send(UserInputEvent::SetSpeakerVolume(new_volume))
                        .is_ok()
                }
            }
            MainCursor::Mic => {
                let new_volume = add_in_bounds(self.mic_volume, 0, 999, delta);
                if new_volume == self.mic_volume {
                    false
                } else {
                    self.user_action_sender
                        .send(UserInputEvent::SetMicVolume(new_volume))
                        .is_ok()
                }
            }
            MainCursor::Peer(_) => {
                if let Some(peer) = self.selected_peer() {
                    let new_volume = add_in_bounds(peer.volume, 0, 999, delta);
                    if new_volume == peer.volume {
                        false
                    } else {
                        let peer_id = peer.id.clone();
                        self.user_action_sender
                            .send(UserInputEvent::SetVolume(peer_id, new_volume))
                            .is_ok()
                    }
                } else {
                    false
                }
            }
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
            if count == 0 {
                assert_eq!(draws, HI_PRI_AT.len());
                continue;
            }
            assert!(
                draws <= TICKS + HI_PRI_AT.len(),
                "N={count}: {draws} draws exceed tick + hi-pri budget"
            );
            let old_behavior = count * TICKS + HI_PRI_AT.len();
            assert!(
                draws < old_behavior,
                "N={count}: {draws} draws show no coalescing vs {old_behavior} before"
            );
        }
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
        assert!(app.process_event(AppEvent::Down));
        assert!(app.process_event(AppEvent::Up));
        let (mut empty, _) = test_app_with_peers(0);
        // With no peers the chain is Speaker <-> Mic, clamped at both ends.
        assert!(empty.process_event(AppEvent::Down));
        assert!(!empty.process_event(AppEvent::Down));
        assert!(empty.process_event(AppEvent::Up));
        assert!(!empty.process_event(AppEvent::Up));
        assert!(empty.process_event(AppEvent::Character('G')));
        assert!(!empty.process_event(AppEvent::Character('G')));
        assert!(!empty.process_event(AppEvent::Character('j')));
        assert!(empty.process_event(AppEvent::Character('k')));
    }

    fn test_app_on_settings() -> (App, UnboundedReceiver<UserInputEvent>) {
        let (tx, rx) = unbounded_channel();
        let mut app = App::new(tx);
        assert!(app.process_event(AppEvent::NextTab));
        assert!(app.process_event(AppEvent::NextTab));
        assert_eq!(app.tab_index, TAB_IDX_SETTINGS);
        (app, rx)
    }

    fn fixture_inputs() -> Vec<(String, String)> {
        vec![
            ("mic-0".to_string(), "Mic 0".to_string()),
            ("mic-1".to_string(), "Mic 1".to_string()),
            ("mic-2".to_string(), "Mic 2".to_string()),
        ]
    }

    fn fixture_outputs() -> Vec<(String, String)> {
        vec![
            ("spk-0".to_string(), "Speakers 0".to_string()),
            ("spk-1".to_string(), "Speakers 1".to_string()),
        ]
    }

    #[test]
    fn device_lists_store_and_clamp_on_shrink() {
        let (mut app, _rx) = test_app_on_settings();
        assert!(app.process_event(AppEvent::SetInputDevices(fixture_inputs())));
        assert!(app.process_event(AppEvent::SetOutputDevices(fixture_outputs())));
        assert!(!app.process_event(AppEvent::SetInputDevices(fixture_inputs())));
        assert!(app.process_event(AppEvent::Character('j')));
        assert!(app.process_event(AppEvent::Character('j')));
        assert_eq!(app.device_cursor, DeviceCursor::Input(2));
        assert!(app.process_event(AppEvent::SetInputDevices(fixture_inputs()[..1].to_vec())));
        assert_eq!(app.device_cursor, DeviceCursor::Input(0));
    }

    #[test]
    fn device_cursor_parks_on_empty_lists() {
        let (mut app, _rx) = test_app_on_settings();
        assert!(!app.process_event(AppEvent::Enter));
        assert!(!app.process_event(AppEvent::Character('j')));
        assert!(!app.process_event(AppEvent::Character('k')));
        assert!(!app.process_event(AppEvent::Down));
        assert!(!app.process_event(AppEvent::Up));
        assert_eq!(app.device_cursor, DeviceCursor::Input(0));
    }

    fn test_settings_app(input_count: usize, output_count: usize) -> App {
        let (tx, _rx) = unbounded_channel();
        let mut app = App::new(tx);
        app.process_event(AppEvent::NextTab);
        app.process_event(AppEvent::NextTab);
        let inputs = (0..input_count)
            .map(|i| (format!("mic-{i}"), format!("Mic {i}")))
            .collect();
        let outputs = (0..output_count)
            .map(|i| (format!("spk-{i}"), format!("Speakers {i}")))
            .collect();
        app.process_event(AppEvent::SetInputDevices(inputs));
        app.process_event(AppEvent::SetOutputDevices(outputs));
        app
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[test]
    fn settings_picker_shows_all_devices_unclipped() {
        let app = test_settings_app(3, 2);
        let mut terminal = test_terminal();
        app.render(&mut terminal).expect("render");
        let text = buffer_text(&terminal).join("\n");
        for name in ["Mic 0", "Mic 1", "Mic 2", "Speakers 0", "Speakers 1"] {
            assert!(text.contains(name), "{name} clipped from settings picker");
        }
    }

    #[test]
    fn main_cursor_walks_seamless_chain() {
        let (mut app, _) = test_app_with_peers(2);
        assert_eq!(app.main_cursor, MainCursor::Speaker);
        assert!(app.process_event(AppEvent::Down));
        assert_eq!(app.main_cursor, MainCursor::Mic);
        assert!(app.process_event(AppEvent::Down));
        assert_eq!(app.main_cursor, MainCursor::Peer(0));
        assert!(app.process_event(AppEvent::Down));
        assert_eq!(app.main_cursor, MainCursor::Peer(1));
        // Clamped at the bottom.
        assert!(!app.process_event(AppEvent::Down));
        assert_eq!(app.main_cursor, MainCursor::Peer(1));
        assert!(app.process_event(AppEvent::Up));
        assert!(app.process_event(AppEvent::Up));
        assert_eq!(app.main_cursor, MainCursor::Mic);
        assert!(app.process_event(AppEvent::Up));
        assert_eq!(app.main_cursor, MainCursor::Speaker);
        // Clamped at the top.
        assert!(!app.process_event(AppEvent::Up));
        // j/k mirror the arrows.
        assert!(app.process_event(AppEvent::Character('j')));
        assert_eq!(app.main_cursor, MainCursor::Mic);
        assert!(app.process_event(AppEvent::Character('k')));
        assert_eq!(app.main_cursor, MainCursor::Speaker);
    }

    #[test]
    fn main_cursor_top_bottom_keys_span_chain() {
        let (mut app, _) = test_app_with_peers(2);
        assert!(app.process_event(AppEvent::Character('G')));
        assert_eq!(app.main_cursor, MainCursor::Peer(1));
        assert!(!app.process_event(AppEvent::Character('G')));
        assert!(app.process_event(AppEvent::Character('g')));
        assert_eq!(app.main_cursor, MainCursor::Speaker);
        assert!(!app.process_event(AppEvent::Character('g')));
    }

    #[test]
    fn main_cursor_falls_back_when_peers_empty() {
        let (mut app, ids) = test_app_with_peers(1);
        assert!(app.process_event(AppEvent::Down));
        assert!(app.process_event(AppEvent::Down));
        assert_eq!(app.main_cursor, MainCursor::Peer(0));
        assert!(app.process_event(AppEvent::RemovePeer(ids[0].clone())));
        assert_eq!(app.main_cursor, MainCursor::Mic);
        // Bottom key with no peers lands on Mic.
        assert!(app.process_event(AppEvent::Character('g')));
        assert!(app.process_event(AppEvent::Character('G')));
        assert_eq!(app.main_cursor, MainCursor::Mic);
    }

    #[test]
    fn plus_minus_target_focused_row() {
        let (mut app, ids) = test_app_with_peers(1);
        let mut rx = {
            let (tx, rx) = unbounded_channel();
            app.user_action_sender = tx;
            rx
        };
        // Speaker row focused initially.
        assert!(app.process_event(AppEvent::Character('+')));
        assert_eq!(rx.try_recv(), Ok(UserInputEvent::SetSpeakerVolume(101)));
        assert!(app.process_event(AppEvent::Character('-')));
        assert_eq!(rx.try_recv(), Ok(UserInputEvent::SetSpeakerVolume(99)));
        // Mic row.
        assert!(app.process_event(AppEvent::Down));
        assert!(app.process_event(AppEvent::Character('+')));
        assert_eq!(rx.try_recv(), Ok(UserInputEvent::SetMicVolume(101)));
        // Peer row still routes to per-peer volume.
        assert!(app.process_event(AppEvent::Down));
        assert_eq!(app.main_cursor, MainCursor::Peer(0));
        assert!(app.process_event(AppEvent::Character('+')));
        assert_eq!(
            rx.try_recv(),
            Ok(UserInputEvent::SetVolume(ids[0].clone(), 101))
        );
        // Peer-only keys are inert on volume rows.
        assert!(app.process_event(AppEvent::Character('g')));
        assert!(!app.process_event(AppEvent::Character(' ')));
        assert!(rx.try_recv().is_err());
        assert!(!app.process_event(AppEvent::Character('d')));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn mute_self_is_independent_of_mic_volume() {
        let (tx, mut rx) = unbounded_channel();
        let mut app = App::new(tx);
        // Mute, then adjust mic gain while muted: mute state must not move.
        assert!(app.process_event(AppEvent::MuteSelf(true)));
        assert!(app.process_event(AppEvent::Down));
        assert_eq!(app.main_cursor, MainCursor::Mic);
        assert!(app.process_event(AppEvent::Character('+')));
        assert_eq!(rx.try_recv(), Ok(UserInputEvent::SetMicVolume(101)));
        assert!(app.mute_self);
        assert!(app.process_event(AppEvent::SetMicVolume(101)));
        assert_eq!(app.mic_volume, 101);
        assert!(app.mute_self);
        // Unmuting restores the untouched gain.
        assert!(app.process_event(AppEvent::MuteSelf(false)));
        assert!(!app.mute_self);
        assert_eq!(app.mic_volume, 101);
    }

    #[test]
    fn master_volume_echo_updates_state() {
        let (mut app, _) = test_app_with_peers(0);
        assert!(!app.process_event(AppEvent::SetSpeakerVolume(100)));
        assert!(app.process_event(AppEvent::SetSpeakerVolume(120)));
        assert_eq!(app.speaker_volume, 120);
        assert!(!app.process_event(AppEvent::SetMicVolume(100)));
        assert!(app.process_event(AppEvent::SetMicVolume(80)));
        assert_eq!(app.mic_volume, 80);
    }

    #[test]
    fn main_tab_renders_volume_and_peers_sections() {
        let (app, _) = test_app_with_peers(1);
        let mut terminal = test_terminal();
        app.render(&mut terminal).expect("render");
        let text = buffer_text(&terminal).join("\n");
        for needle in ["Main", "Volume", "Speaker volume", "Mic volume", "Peers"] {
            assert!(text.contains(needle), "{needle} missing from main tab");
        }
    }

    #[test]
    fn main_tab_volume_styles_and_mute_indicator() {
        use ratatui::style::{Color, Modifier};

        fn cell_rows(
            terminal: &Terminal<TestBackend>,
        ) -> Vec<(Vec<char>, Vec<Color>, Vec<Modifier>)> {
            let buffer = terminal.backend().buffer();
            let width = buffer.area.width as usize;
            buffer
                .content
                .chunks(width)
                .map(|row| {
                    (
                        row.iter()
                            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
                            .collect(),
                        row.iter().map(|cell| cell.fg).collect(),
                        row.iter().map(|cell| cell.modifier).collect(),
                    )
                })
                .collect()
        }

        fn label_cells(
            rows: &[(Vec<char>, Vec<Color>, Vec<Modifier>)],
            label: &str,
        ) -> (Vec<Color>, Vec<Modifier>) {
            let needle: Vec<char> = label.chars().collect();
            let (_, fgs, mods) = rows
                .iter()
                .find(|(chars, _, _)| chars.windows(needle.len()).any(|w| w == needle.as_slice()))
                .unwrap_or_else(|| panic!("{label} not rendered"));
            let start = rows
                .iter()
                .find_map(|(chars, _, _)| {
                    chars
                        .windows(needle.len())
                        .position(|w| w == needle.as_slice())
                })
                .expect("label position");
            (
                fgs[start..start + needle.len()].to_vec(),
                mods[start..start + needle.len()].to_vec(),
            )
        }

        fn row_chars_containing(
            rows: &[(Vec<char>, Vec<Color>, Vec<Modifier>)],
            label: &str,
        ) -> Vec<char> {
            let needle: Vec<char> = label.chars().collect();
            rows.iter()
                .find(|(chars, _, _)| chars.windows(needle.len()).any(|w| w == needle.as_slice()))
                .map(|(chars, _, _)| chars.clone())
                .unwrap_or_else(|| panic!("{label} not rendered"))
        }

        let (mut app, _) = test_app_with_peers(1);
        app.own_display_name = Some("me".to_string());

        // Unmuted: DarkGray labels, white values, speaker icon on mic row.
        let mut terminal = test_terminal();
        app.render(&mut terminal).expect("render");
        let all_text = buffer_text(&terminal).join("\n");
        assert!(all_text.contains("(you)"));
        assert!(!all_text.contains("MUTED"));
        let rows = cell_rows(&terminal);
        for label in ["Speaker volume", "Mic volume"] {
            let (fgs, mods) = label_cells(&rows, label);
            assert!(
                fgs.iter().all(|fg| *fg == Color::DarkGray),
                "{label} label not DarkGray"
            );
            assert!(
                mods.iter().all(|m| !m.contains(Modifier::CROSSED_OUT)),
                "{label} label crossed out while unmuted"
            );
            let row = row_chars_containing(&rows, label);
            assert!(row.contains(&'1'), "{label} row shows no volume value");
        }
        let mic_row = row_chars_containing(&rows, "Mic volume");
        assert!(mic_row.contains(&'🔊'), "mic row shows no unmute icon");

        // Muted: crossed-out mic label + mute icon; speaker row untouched.
        assert!(app.process_event(AppEvent::MuteSelf(true)));
        let mut terminal = test_terminal();
        app.render(&mut terminal).expect("render");
        let all_text = buffer_text(&terminal).join("\n");
        assert!(all_text.contains("(you)"));
        assert!(!all_text.contains("MUTED"));
        let rows = cell_rows(&terminal);
        let (mic_fgs, mic_mods) = label_cells(&rows, "Mic volume");
        assert!(mic_fgs.iter().all(|fg| *fg == Color::DarkGray));
        assert!(
            mic_mods.iter().all(|m| m.contains(Modifier::CROSSED_OUT)),
            "mic label not crossed out while muted"
        );
        let mic_row = row_chars_containing(&rows, "Mic volume");
        assert!(mic_row.contains(&'🔇'), "mic row shows no mute icon");
        let (_, speaker_mods) = label_cells(&rows, "Speaker volume");
        assert!(
            speaker_mods
                .iter()
                .all(|m| !m.contains(Modifier::CROSSED_OUT)),
            "speaker label crossed out by mute"
        );
    }
}
