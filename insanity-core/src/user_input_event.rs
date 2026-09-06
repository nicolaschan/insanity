#[derive(Debug, PartialEq, Eq)]
pub enum UserInputEvent {
    DisablePeer(String),
    EnablePeer(String),
    SetDenoise(String, DenoiseSelection),
    SetVolume(String, usize),
    SendMessage(String),
    SetMuteSelf(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DenoiseSelection {
    None,
    #[default]
    Nnnoiseless,
}

impl DenoiseSelection {
    pub fn next(&self) -> Self {
        match self {
            DenoiseSelection::None => DenoiseSelection::Nnnoiseless,
            DenoiseSelection::Nnnoiseless => DenoiseSelection::None,
        }
    }
}
