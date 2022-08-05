use tui::{
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::DOT,
    text::{Span, Spans},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, Tabs, Widget},
    Frame,
};

use crate::{App, Editor, Peer, TAB_IDX_CHAT, TAB_IDX_PEERS, TAB_IDX_SETTINGS};

const BG_GRAY: Color = Color::Rgb(50, 50, 50);
const SELECTED: Color = Color::Rgb(80, 80, 80);
const CONNECTED: Color = Color::Rgb(0, 255, 0);
const CONNECTING: Color = Color::Rgb(0, 255, 255);

// Gruvbox (mostly) dark theme
const COLOR_RED: Color    = Color::Rgb(0xfb, 0x49, 0x34); // Color::Rgb(0xcc, 0x24, 0x1d);
const COLOR_GREEN: Color  = Color::Rgb(0x98, 0x98, 0x1a);
const COLOR_YELLOW: Color = Color::Rgb(0xd7, 0x99, 0x21);
const COLOR_BLUE: Color   = Color::Rgb(0x45, 0x85, 0x88);
const COLOR_PURPLE: Color = Color::Rgb(0xb1, 0x62, 0x86);
const COLOR_AQUA: Color   = Color::Rgb(0x68, 0x96, 0x6a);
const COLOR_ORANGE: Color = Color::Rgb(0xd6, 0x5d, 0x0e);
const NUM_CHAT_COLORS: usize = 7;
const CHAT_COLORS: [Color; NUM_CHAT_COLORS] = [
    COLOR_RED,
    COLOR_GREEN,
    COLOR_YELLOW,
    COLOR_BLUE,
    COLOR_PURPLE,
    COLOR_AQUA,
    COLOR_ORANGE,
];

fn default_block<'a>() -> Block<'a> {
    Block::default()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BG_GRAY))
        .borders(Borders::ALL)
}

pub fn ui<B: Backend>(f: &mut Frame<B>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(f.size());
    f.render_widget(tab_list(app), chunks[0]);
    match app.tab_index {
        TAB_IDX_PEERS => render_peer_list(f, app, chunks[1]),
        TAB_IDX_CHAT => render_chat(f, app, chunks[1]),
        TAB_IDX_SETTINGS => render_settings(f, app, chunks[1]),
        _ => panic!("Tab index out of bounds."),
    }
}

fn tab_list(app: &App) -> impl Widget {
    let titles = app
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
            Spans::from(Span::styled(tab_name, style))
        })
        .collect();

    Tabs::new(titles)
        .block(default_block())
        .select(app.tab_index)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(Style::default().fg(Color::LightBlue))
        .divider(Span::styled(DOT, Style::default().fg(BG_GRAY)))
}

fn peer_row<'a>(peer: &Peer, selected: bool) -> Row<'a> {
    let style = if selected {
        Style::default().bg(SELECTED)
    } else {
        Style::default()
    };

    let attributes = Cell::from(Spans::from(vec![Span::styled(
        format!("{}", peer.volume),
        Style::default().fg(if peer.denoised {
            Color::White
        } else {
            Color::DarkGray
        }),
    )]));

    match &peer.state {
        crate::PeerState::Connected(address) => Row::new(vec![
            Cell::from("✓"),
            attributes,
            Cell::from(match peer.display_name.as_ref() {
                Some(name) => name.clone(),
                None => peer.id.clone(),
            })
            .style(style),
            Cell::from(format!("@{}", address)).style(Style::default().fg(Color::DarkGray)),
        ])
        .style(Style::default().fg(CONNECTED)),
        crate::PeerState::Disconnected => Row::new(vec![
            Cell::from(" "),
            attributes,
            Cell::from(peer.id.clone()).style(style),
        ])
        .style(Style::default().fg(Color::DarkGray)),
        crate::PeerState::Disabled => Row::new(vec![
            Cell::from("✗"),
            attributes,
            Cell::from(Span::styled(
                peer.id.clone(),
                Style::default().add_modifier(Modifier::CROSSED_OUT),
            ))
            .style(style),
        ])
        .style(Style::default().fg(Color::DarkGray)),
        crate::PeerState::Connecting(address) => Row::new(vec![
            Cell::from(DOT),
            attributes,
            Cell::from(match peer.display_name.as_ref() {
                Some(name) => name.clone(),
                None => peer.id.clone(),
            })
            .style(style),
            Cell::from(format!("@{}", address)).style(Style::default().fg(Color::DarkGray)),
        ])
        .style(Style::default().fg(CONNECTING)),
    }
}

fn render_peer_list<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .peers
        .values()
        .enumerate()
        .map(|(i, peer)| peer_row(peer, i == app.peer_index))
        .collect();
    let widget = Table::new(rows)
        .style(Style::default().fg(Color::White))
        .widths(&[
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(70),
            Constraint::Min(24),
        ])
        .column_spacing(1)
        .block(default_block());
    f.render_widget(widget, area);
}

fn render_editor(editor: &Editor) -> Paragraph {
    let before_cursor: String = editor.buffer.chars().take(editor.cursor).collect();
    let at_cursor: String = editor
        .buffer
        .chars()
        .nth(editor.cursor)
        .unwrap_or(' ')
        .to_string();
    let after_cursor: String = editor.buffer.chars().skip(editor.cursor + 1).collect();
    let text = vec![Spans::from(vec![
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
    let mut text: Vec<Vec<tui::text::Spans>> = vec![];
    let mut total_line_count = 0;
    for (address, message_text) in chat_history.iter().rev().skip(chat_offset) {
        let name_color = CHAT_COLORS[(hash(address) % (NUM_CHAT_COLORS as u64)) as usize];
        let name_style = Style::default().fg(name_color);
        let display_name = if let Some(peer) = peers.get(address) {
            peer.display_name.as_ref().unwrap_or(address)
        } else if **own_address.as_ref().unwrap() == *address {
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
            .iter()
            .nth(display_name_num_wraps)
            .unwrap()
            .split_at(display_name.len() - name_count);
        let split_line = vec![Spans::from(vec![
            Span::styled(name_part.to_string(), name_style),
            Span::raw(text_part.to_string()),
        ])];

        let lines: Vec<Spans> = lines
            .iter()
            .take(display_name_num_wraps)
            .map(|s| Spans::from(vec![Span::styled(s.to_string(), name_style)]))
            .chain(split_line.into_iter())
            .chain(
                lines
                    .iter()
                    .skip(display_name_num_wraps + 1)
                    .map(|s| Spans::from(vec![Span::raw(s.to_string())])),
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
    let text: Vec<Spans> = text.into_iter().flatten().collect();
    Paragraph::new(text)
}

fn render_chat<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(area);
    let editor_widget = render_editor(&app.editor).block(default_block());
    let chat_history_widget = render_chat_history(
            &app.chat_history,
            app.chat_offset,
            &app.peers,
            &app.own_address,
            &app.own_display_name,
            &chunks[1],
        )   
        .block(default_block());
    f.render_widget(editor_widget, chunks[0]);
    f.render_widget(chat_history_widget, chunks[1]);
}

fn render_settings<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(area);
    let widget = Paragraph::new(vec![
            match app.own_address.as_ref() {
                Some(addr) => Spans::from(vec![
                    Span::styled("Your address: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(addr.to_string(), Style::default().fg(Color::LightBlue)),
                ]),
                None => Spans::from(vec![Span::styled(
                    "Waiting for tor...".to_string(),
                    Style::default().fg(Color::DarkGray),
                )]),
            },
        ])
        .block(default_block())
        .style(Style::default().fg(Color::White));
    f.render_widget(widget, chunks[0]);
}
