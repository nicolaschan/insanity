use insanity_core::built_info;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{App, components::block::default_block};

pub fn render_settings(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    let server_widget = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Bridge servers: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.servers.join(", "),
                Style::default().fg(Color::LightBlue),
            ),
        ]),
        match app.room.as_ref() {
            Some(room) => Line::from(vec![
                Span::styled("Room: ", Style::default().fg(Color::DarkGray)),
                Span::styled(room.to_string(), Style::default().fg(Color::LightBlue)),
            ]),
            None => Line::from(vec![Span::styled(
                "Room: no room specified...".to_string(),
                Style::default().fg(Color::DarkGray),
            )]),
        },
        match app.room_fingerprint.as_ref() {
            Some(room_fingerprint) => Line::from(vec![
                Span::styled("Room fingerprint: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    room_fingerprint.to_string(),
                    Style::default().fg(Color::LightBlue),
                ),
            ]),
            None => Line::from(vec![Span::styled(
                "Room fingerprint: no room fingerprint...".to_string(),
                Style::default().fg(Color::DarkGray),
            )]),
        },
        match app.own_public_key.as_ref() {
            Some(key) => Line::from(vec![
                Span::styled("Your public key: ", Style::default().fg(Color::DarkGray)),
                Span::styled(key.to_string(), Style::default().fg(Color::LightBlue)),
            ]),
            None => Line::from(vec![Span::styled(
                "Your public key: waiting to connect to server...".to_string(),
                Style::default().fg(Color::DarkGray),
            )]),
        },
    ])
    .block(default_block())
    .style(Style::default().fg(Color::White));
    f.render_widget(server_widget, chunks[0]);

    let version_widget = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Version: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                built_info::GIT_VERSION.unwrap_or("unknown"),
                Style::default().fg(Color::LightBlue),
            ),
        ]),
        Line::from(vec![
            Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
            Span::styled(built_info::TARGET, Style::default().fg(Color::LightBlue)),
        ]),
    ])
    .block(default_block())
    .style(Style::default().fg(Color::White));
    f.render_widget(version_widget, chunks[1]);

    let audio_widget = audio_widget(app.input_device_name.clone());
    f.render_widget(audio_widget, chunks[2]);
}

fn audio_widget<'a>(input_device_name: String) -> Paragraph<'a> {
    Paragraph::new(vec![Line::from(vec![
        Span::styled(
            "Current input device: ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(input_device_name, Style::default().fg(Color::LightBlue)),
    ])])
    .block(default_block())
    .style(Style::default().fg(Color::White))
}
