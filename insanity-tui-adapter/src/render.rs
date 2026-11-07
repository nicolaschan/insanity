use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::DOT,
    text::{Line, Span},
    widgets::{Block, Cell, Paragraph, Row, Table, Tabs, Widget},
};

use crate::{
    App, DECREMENT_PEER_VOLUME_KEY, Editor, INCREMENT_PEER_VOLUME_KEY, MUTE_KEY, MainCursor, Peer,
    TAB_IDX_CHAT, TAB_IDX_MAIN, TAB_IDX_SETTINGS, TOGGLE_PEER_DENOISE_KEY, TOGGLE_PEER_KEY,
    components::{block::default_block, settings::render_settings},
    style::{BG_GRAY, CHAT_COLORS, COLOR_RED, CONNECTED, NUM_CHAT_COLORS, SELECTED},
};

pub(crate) fn loudness_bucket(display_name: &str, loudness: f64) -> usize {
    (display_name.chars().count() as f64 * loudness.clamp(0.0, 1.0)) as usize
}

pub fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(f.area());
    f.render_widget(tab_list(app), chunks[0]);
    match app.tab_index {
        TAB_IDX_MAIN => render_main(f, app, chunks[1]),
        TAB_IDX_CHAT => render_chat(f, app, chunks[1]),
        TAB_IDX_SETTINGS => render_settings(f, app, chunks[1]),
        _ => panic!("Tab index out of bounds."),
    }
}

fn tab_list(app: &App) -> impl Widget + use<> {
    let titles: Vec<Line> = app
        .tabs
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, tab_name)| {
            let style = if i == TAB_IDX_CHAT && app.unread_messages {
                Style::default()
                    .fg(COLOR_RED)
                    .add_modifier(Modifier::RAPID_BLINK)
            } else {
                Style::default()
            };
            Line::from(Span::styled(tab_name, style))
        })
        .collect();

    Tabs::new(titles)
        .block(default_block())
        .select(app.tab_index)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(Style::default().fg(Color::LightBlue))
        .divider(Span::styled(DOT, Style::default().fg(BG_GRAY)))
}

fn peer_row<'a>(peer: &'a Peer, selected: bool) -> Row<'a> {
    let style = if selected {
        Style::default().bg(SELECTED)
    } else {
        Style::default()
    };

    let denoise_symbol = match peer.denoised {
        insanity_core::user_input_event::DenoiseSelection::None => "🫨",
        insanity_core::user_input_event::DenoiseSelection::Nnnoiseless => "🤫",
    };

    let attributes = Cell::from(Line::from(vec![Span::styled(
        format!("{}", peer.volume),
        Style::default().fg(match peer.state {
            crate::PeerState::Connected(_) => Color::White,
            _ => Color::DarkGray,
        }),
    )]));

    let display_name: &str = peer.display_name.as_ref().unwrap_or(&peer.id);

    match peer.state {
        crate::PeerState::Connected(ref address) => {
            let loudness_length = loudness_bucket(display_name, peer.loudness);
            let display_name_with_loudness_bg = display_name
                .chars()
                .take(loudness_length)
                .collect::<String>();
            let display_name_normal_bg = display_name
                .chars()
                .skip(loudness_length)
                .collect::<String>();

            Row::new(vec![
                Cell::from(denoise_symbol),
                attributes,
                Cell::from(Line::from(vec![
                    Span::styled(display_name_with_loudness_bg, style.fg(Color::Yellow)),
                    Span::styled(display_name_normal_bg, style.fg(CONNECTED)),
                    Span::styled(" <-> ", style.fg(Color::DarkGray)),
                    Span::styled(address.as_str(), style.fg(Color::Cyan)),
                ]))
                .style(style),
            ])
        }
        crate::PeerState::Disconnected => Row::new(vec![
            Cell::from(denoise_symbol),
            attributes,
            Cell::from(display_name).style(style),
        ])
        .style(Style::default().fg(Color::DarkGray)),
        crate::PeerState::Disabled => Row::new(vec![
            Cell::from(denoise_symbol),
            attributes,
            Cell::from(Span::styled(
                display_name,
                Style::default().add_modifier(Modifier::CROSSED_OUT),
            ))
            .style(style.fg(Color::DarkGray)),
        ]),
        crate::PeerState::Connecting(ref address) => Row::new(vec![
            Cell::from(denoise_symbol),
            attributes,
            Cell::from(Line::from(vec![
                Span::styled(display_name, style.fg(Color::DarkGray)),
                Span::styled(" --> ", style.fg(Color::DarkGray)),
                Span::styled(address.clone(), style.fg(Color::DarkGray)),
            ]))
            .style(style),
        ]),
    }
}

fn char_to_readable(c: char) -> String {
    match c {
        ' ' => "space".to_string(),
        '\t' => "tab".to_string(),
        _ => c.to_string(),
    }
}

fn main_command_help_entry(key: char, help_str: &'static str) -> String {
    format!("[{}] {}     ", char_to_readable(key), help_str)
}

static MAIN_HELP_TEXT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    [
        ('\t', "tab"),
        (TOGGLE_PEER_KEY, "toggle peer"),
        (TOGGLE_PEER_DENOISE_KEY, "toggle denoise"),
        (INCREMENT_PEER_VOLUME_KEY, "volume up"),
        (DECREMENT_PEER_VOLUME_KEY, "volume down"),
        (MUTE_KEY, "toggle self mute"),
    ]
    .iter()
    .map(|(key, help_str)| main_command_help_entry(*key, help_str))
    .fold("   ".to_string(), |acc, x| acc + &x)
});

fn volume_row<'a>(
    icon: &'a str,
    label: &'static str,
    volume: usize,
    selected: bool,
    muted: bool,
) -> Row<'a> {
    let label_style = {
        let mut style = Style::default().fg(Color::DarkGray);
        if selected {
            style = style.bg(SELECTED);
        }
        if muted {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    };
    Row::new(vec![
        Cell::from(icon),
        Cell::from(Line::from(vec![Span::styled(
            format!("{volume}"),
            Style::default(),
        )])),
        Cell::from(Line::from(vec![Span::styled(label, label_style)])),
    ])
}

fn render_main(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let (speaker_cursor, mic_cursor, peer_cursor) = match app.main_cursor {
        MainCursor::Speaker => (Some(0), None, None),
        MainCursor::Mic => (None, Some(0), None),
        MainCursor::Peer(i) => (None, None, Some(i)),
    };

    let volume_rows = vec![
        volume_row(
            "",
            "Speaker volume",
            app.speaker_volume,
            speaker_cursor.is_some(),
            false,
        ),
        volume_row(
            if app.mute_self { "🔇" } else { "🔊" },
            "Mic volume",
            app.mic_volume,
            mic_cursor.is_some(),
            app.mute_self,
        ),
    ];
    let mut volume_block = default_block().title("Volume");
    if speaker_cursor.is_some() || mic_cursor.is_some() {
        volume_block = volume_block.border_style(Style::default().fg(Color::DarkGray));
    }

    let self_row = match &app.own_display_name {
        Some(display_name) => vec![Row::new(vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from(Line::from(vec![
                Span::styled(display_name, Style::default().fg(Color::Magenta)),
                Span::styled(" (you)", Style::default().fg(Color::DarkGray)),
            ])),
        ])],
        None => vec![],
    };
    let volume_widget = Table::new(
        self_row.into_iter().chain(volume_rows).collect::<Vec<_>>(),
        [
            Constraint::Min(2),
            Constraint::Length(3),
            Constraint::Percentage(100),
        ],
    )
    .style(Style::default().fg(Color::White))
    .column_spacing(1)
    .block(volume_block);
    f.render_widget(volume_widget, chunks[0]);

    let rows: Vec<Row> = app
        .peers
        .values()
        .enumerate()
        .map(|(i, peer)| peer_row(peer, peer_cursor == Some(i)))
        .collect();

    let mut peers_block = default_block().title("Peers");
    if peer_cursor.is_some() {
        peers_block = peers_block.border_style(Style::default().fg(Color::DarkGray));
    }
    let peer_list = Table::new(
        rows,
        [
            Constraint::Min(2),
            Constraint::Length(3),
            Constraint::Percentage(100),
        ],
    )
    .style(Style::default().fg(Color::White))
    .column_spacing(1)
    .block(peers_block);
    f.render_widget(peer_list, chunks[1]);

    // Command help list
    let commands = Paragraph::new(MAIN_HELP_TEXT.as_str())
        .block(Block::default().style(Style::default().fg(Color::DarkGray)));
    f.render_widget(commands, chunks[2]);
}

fn render_editor<'a>(editor: &'a Editor, area: &'a Rect) -> Paragraph<'a> {
    let max_text_width = area.width.saturating_sub(2) as usize;
    let mut remaining_width = max_text_width;
    let before_cursor: String = editor
        .buffer
        .chars()
        .take(std::cmp::min(editor.cursor, max_text_width))
        .collect();
    remaining_width -= before_cursor.len();
    let at_cursor: String = editor
        .buffer
        .chars()
        .nth(editor.cursor)
        .unwrap_or(' ')
        .to_string();
    remaining_width -= at_cursor.len();
    let after_cursor: String = editor
        .buffer
        .chars()
        .skip(editor.cursor + 1)
        .take(remaining_width)
        .collect();
    let text = vec![Line::from(vec![
        Span::raw(before_cursor),
        Span::styled(
            at_cursor,
            Style::default().fg(Color::Black).bg(Color::White),
        ),
        Span::raw(after_cursor),
    ])];
    Paragraph::new(text)
}

fn hash<T: std::hash::Hash>(object: &T) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut hasher = DefaultHasher::new();
    object.hash(&mut hasher);
    hasher.finish()
}

fn render_chat_history<'a>(
    chat_history: &'a [(String, String)],
    chat_offset: usize,
    peers: &'a std::collections::BTreeMap<String, Peer>,
    own_address: &'a Option<String>,
    own_display_name: &'a Option<String>,
    area: &'a Rect,
) -> Paragraph<'a> {
    let max_text_width = area.width.saturating_sub(2) as usize;
    let max_num_lines = area.height.saturating_sub(2) as usize;
    let mut text: Vec<Vec<Line>> = vec![];
    let mut total_line_count = 0;
    for (address, message_text) in chat_history.iter().rev().skip(chat_offset) {
        let name_color = CHAT_COLORS[(hash(address) % (NUM_CHAT_COLORS as u64)) as usize];
        let name_style = Style::default().fg(name_color);
        let display_name = if let Some(peer) = peers.get(address) {
            peer.display_name.as_ref().unwrap_or(address)
        } else if own_address.is_some() && **own_address.as_ref().unwrap() == *address {
            own_display_name.as_ref().unwrap_or(address)
        } else {
            address
        };

        let message = display_name.to_string() + ": " + message_text;
        let lines = textwrap::wrap(&message, max_text_width);

        let mut name_count = 0;
        // Number of full lines the display name takes.
        let display_name_num_wraps = lines
            .iter()
            .take_while(|s| {
                if s.len() < display_name.len() - name_count {
                    name_count += s.len();
                    true
                } else {
                    false
                }
            })
            .count();

        // Style the line that is part display name and part message content.
        let (name_part, text_part) = lines
            .get(display_name_num_wraps)
            .unwrap()
            .split_at(display_name.len() - name_count);
        let split_line = vec![Line::from(vec![
            Span::styled(name_part.to_string(), name_style),
            Span::raw(text_part.to_string()),
        ])];

        let lines: Vec<Line> = lines
            .iter()
            .take(display_name_num_wraps)
            .map(|s| Line::from(vec![Span::styled(s.to_string(), name_style)]))
            .chain(split_line)
            .chain(
                lines
                    .iter()
                    .skip(display_name_num_wraps + 1)
                    .map(|s| Line::from(vec![Span::raw(s.to_string())])),
            )
            .rev()
            .take(max_num_lines - total_line_count)
            .collect();
        total_line_count += lines.len();
        text.push(lines.into_iter().rev().collect());

        if total_line_count >= max_num_lines {
            break;
        }
    }
    text.reverse();
    let text: Vec<Line> = text.into_iter().flatten().collect();
    Paragraph::new(text)
}

fn render_chat(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);
    let chat_history_widget = render_chat_history(
        &app.chat_history,
        app.chat_offset,
        &app.peers,
        &app.own_public_key,
        &app.own_display_name,
        &chunks[0],
    )
    .block(default_block());
    let editor_widget = render_editor(&app.editor, &chunks[1]).block(default_block());
    f.render_widget(chat_history_widget, chunks[0]);
    f.render_widget(editor_widget, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::loudness_bucket;

    #[test]
    fn bucket_quantizes_to_integer_span() {
        assert_eq!(loudness_bucket("0123456789", 0.0), 0);
        assert_eq!(loudness_bucket("0123456789", 0.5), 5);
        assert_eq!(loudness_bucket("0123456789", 1.0), 10);
    }

    #[test]
    fn bucket_counts_chars_not_bytes() {
        assert_eq!(loudness_bucket("a🤫", 1.0), 2);
        assert_eq!(loudness_bucket("a🤫", 0.5), 1);
    }
}
