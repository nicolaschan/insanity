use insanity_core::built_info;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    App, DeviceFocus,
    components::block::default_block,
    style::{BG_GRAY, SELECTED},
};

pub fn render_settings(f: &mut Frame, app: &App, area: Rect) {
    let input_height = (app.input_devices.len().max(1) + 2) as u16;
    let output_height = (app.output_devices.len().max(1) + 2) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(input_height),
            Constraint::Length(output_height),
            Constraint::Length(1),
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

    let (input_cursor, output_cursor) = match app.device_cursor_section() {
        DeviceFocus::Input => (Some(app.device_cursor_row()), None),
        DeviceFocus::Output => (None, Some(app.device_cursor_row())),
    };
    f.render_widget(
        device_section(
            "Input devices",
            &app.input_devices,
            &app.input_device_name,
            input_cursor,
            "No input devices — press r to refresh",
        ),
        chunks[2],
    );
    f.render_widget(
        device_section(
            "Output devices",
            &app.output_devices,
            &app.output_device_name,
            output_cursor,
            "No output devices — press r to refresh",
        ),
        chunks[3],
    );
    let hints = Paragraph::new(Line::from(vec![Span::styled(
        "Enter select · r refresh",
        Style::default().fg(Color::DarkGray),
    )]));
    f.render_widget(hints, chunks[4]);
}

fn device_section<'a>(
    title: &'static str,
    devices: &'a [(String, String)],
    current_name: &'a str,
    cursor: Option<usize>,
    empty_label: &'static str,
) -> Paragraph<'a> {
    let mut lines = Vec::new();
    if devices.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            empty_label,
            Style::default().fg(Color::DarkGray),
        )]));
    }
    for (index, (_, name)) in devices.iter().enumerate() {
        let cursor_here = cursor == Some(index);
        let mut spans = vec![
            Span::styled(
                if cursor_here { "> " } else { "  " },
                Style::default().fg(Color::White),
            ),
            Span::styled(name.as_str(), Style::default().fg(Color::LightBlue)),
        ];
        if name.as_str() == current_name {
            spans.push(Span::styled(
                " (current)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        let line = Line::from(spans);
        lines.push(if cursor_here {
            line.style(Style::default().bg(SELECTED))
        } else {
            line
        });
    }
    let border = if cursor.is_some() { SELECTED } else { BG_GRAY };
    Paragraph::new(lines)
        .block(
            default_block()
                .title(title)
                .border_style(Style::default().fg(border)),
        )
        .style(Style::default().fg(Color::White))
}
