use tui::{
    style::Style,
    widgets::{Block, BorderType, Borders},
};

use crate::style::BG_GRAY;

pub fn default_block<'a>() -> Block<'a> {
    Block::default()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BG_GRAY))
        .borders(Borders::ALL)
}
