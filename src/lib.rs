#![feature(test)]

use crate::tui::TuiEvent;
use crossbeam::channel::Sender;

pub mod clerver;
pub mod client;
pub mod processor;
pub mod realtime_buffer;
pub mod server;
pub mod tui;
pub mod protocol;
pub mod coordinator;

#[derive(Clone)]
pub struct InsanityConfig {
    pub denoise: bool,
    pub ui_message_sender: Sender<TuiEvent>,
    pub music: Option<String>,
    pub sample_rate: usize,
    pub channels: usize,
}
