#[derive(Debug, PartialEq, Eq)]
pub enum UserInputEvent {
    DisablePeer(String),
    EnablePeer(String),
    DisableDenoise(String),
    EnableDenoise(String),
    SetVolume(String, usize),
    SendMessage(String),
    SetMuteSelf(bool),
}
