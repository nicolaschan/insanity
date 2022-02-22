#![feature(test)]

use crate::tui::TuiEvent;
use crossbeam::channel::Sender;

pub mod clerver;
pub mod client;
pub mod coordinator;
pub mod processor;
pub mod protocol;
pub mod realtime_buffer;
pub mod server;
pub mod tui;
pub mod resampler;

#[derive(Clone)]
pub struct InsanityConfig {
    pub denoise: bool,
    pub ui_message_sender: Sender<TuiEvent>,
    pub music: Option<String>,
    pub sample_rate: usize,
    pub channels: usize,
}
