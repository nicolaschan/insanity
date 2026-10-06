use insanity_core::built_info;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    App, DeviceCursor, REFRESH_DEVICES_KEY, components::block::default_block, style::SELECTED,
};

pub fn render_settings(f: &mut Frame, app: &App, area: Rect) {
    let input_height = section_height(app.input_devices.len());
    let output_height = section_height(app.output_devices.len());
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

    let (input_cursor, output_cursor) = match app.device_cursor {
        DeviceCursor::Input(row) => (Some(row), None),
        DeviceCursor::Output(row) => (None, Some(row)),
    };
    f.render_widget(
        device_widget(
            section_title("Input devices", &app.default_input_device_name),
            &app.input_devices,
            &app.input_device_name,
            input_cursor,
        ),
        chunks[2],
    );
    f.render_widget(
        device_widget(
            section_title("Output devices", &app.default_output_device_name),
            &app.output_devices,
            &app.output_device_name,
            output_cursor,
        ),
        chunks[3],
    );
    f.render_widget(hints_line(), chunks[4]);
}

fn section_title(base: &str, default_name: &str) -> String {
    if default_name.is_empty() {
        base.to_string()
    } else {
        format!("{base} (default: {default_name})")
    }
}

fn section_height(devices: usize) -> u16 {
    devices.max(1) as u16 + 2
}

fn device_widget<'a>(
    title: String,
    devices: &'a [(String, String)],
    current_name: &str,
    cursor: Option<usize>,
) -> Paragraph<'a> {
    let lines = if devices.is_empty() {
        vec![Line::from(Span::styled(
            "(no devices found)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        devices
            .iter()
            .enumerate()
            .map(|(row, (_, name))| {
                let row_style = {
                    let mut style = Style::default().fg(Color::DarkGray);
                    if cursor == Some(row) {
                        style = style.bg(SELECTED);
                    }
                    if name == current_name {
                        style = style.fg(Color::LightBlue);
                    }
                    style
                };
                let label = if name == current_name {
                    format!("{name} (current)")
                } else {
                    name.clone()
                };
                Line::from(vec![Span::styled(label, row_style)])
            })
            .collect()
    };
    let mut block = default_block().title(title);
    if cursor.is_some() {
        block = block.border_style(Style::default().fg(Color::DarkGray));
    }
    Paragraph::new(lines)
        .block(block)
        .style(Style::default().fg(Color::Gray))
}

fn hints_line() -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        format!("[{REFRESH_DEVICES_KEY}] refresh"),
        Style::default().fg(Color::DarkGray),
    )))
}
