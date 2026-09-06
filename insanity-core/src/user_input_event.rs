use crate::user_input_event::DenoiseSelection::{Deepfilter, Nnnoiseless};

#[derive(Debug, PartialEq, Eq)]
pub enum UserInputEvent {
    DisablePeer(String),
    EnablePeer(String),
    SetDenoise(String, DenoiseSelection),
    SetVolume(String, usize),
    SendMessage(String),
    SetMuteSelf(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum DenoiseSelection {
    None,
    Deepfilter,
    #[default]
    Nnnoiseless,
}


impl DenoiseSelection {
    pub fn next(&self) -> Self {
        match self {
            DenoiseSelection::None => Deepfilter,
            DenoiseSelection::Deepfilter => Nnnoiseless,
            DenoiseSelection::Nnnoiseless => DenoiseSelection::None,
        }
    }
}
