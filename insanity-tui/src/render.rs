use tui::{
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style, Modifier},
    symbols::DOT,
    text::{Span, Spans},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, Tabs, Widget},
    Frame,
};

const BG_GRAY: Color = Color::Rgb(50, 50, 50);
const SELECTED: Color = Color::Rgb(80, 80, 80);
const CONNECTED: Color = Color::Rgb(0, 255, 0);
const CONNECTING: Color = Color::Rgb(0, 255, 255);

fn default_block<'a>() -> Block<'a> {
    Block::default()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BG_GRAY))
        .borders(Borders::ALL)
}

use crate::{App, Editor, Peer};

pub fn ui<B: Backend>(f: &mut Frame<B>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(f.size());
    f.render_widget(tab_list(app), chunks[0]);
    match app.tab_index {
        0 => render_peer_list(f, app, chunks[1]),
        1 => render_chat(f, app, chunks[1]),
        _ => render_settings(f, app, chunks[1]),
    }
    // f.render_widget(Paragraph::new("insanity v2")
    //     .alignment(Alignment::Center)
    //     .style(Style::default().fg(BG_GRAY)),
    //     chunks[2]);
}

fn tab_list(app: &App) -> impl Widget {
    let titles = app.tabs.iter().cloned().map(Spans::from).collect();
    Tabs::new(titles)
        .block(default_block())
        .select(app.tab_index)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(Style::default().fg(Color::LightBlue))
        .divider(Span::styled(DOT, Style::default().fg(BG_GRAY)))
}

fn peer_row<'a>(peer: &Peer, selected: bool) -> Row<'a> {
    let mut style = Style::default();
    if selected {
        style = style.bg(SELECTED);
    }
    let attributes = Cell::from(Spans::from(vec![
        Span::styled(format!("{}", peer.volume.to_string()), 
            Style::default().fg(if peer.denoised { Color::White } else { Color::DarkGray })),
    ]));
    match peer.state.clone() {
        crate::PeerState::Connected(address) => {
            Row::new(vec![
                Cell::from("✓"), 
                attributes,
                Cell::from(match peer.display_name.as_ref() {
                    Some(name) => name.clone(),
                    None => peer.id.clone(),
                }).style(style),
                Cell::from(format!("@{}", address)).style(Style::default().fg(Color::DarkGray)),
                ])
                .style(Style::default().fg(CONNECTED))
        }
        crate::PeerState::Disconnected => {
            Row::new(vec![
                Cell::from(" "), 
                attributes,
                Cell::from(peer.id.clone()).style(style)
            ]).style(Style::default().fg(Color::DarkGray))
        }
        crate::PeerState::Disabled => {
            Row::new(vec![
                Cell::from("✗"), 
                attributes,
                Cell::from(Span::styled(peer.id.clone(), Style::default().add_modifier(Modifier::CROSSED_OUT))).style(style),
            ]).style(Style::default().fg(Color::DarkGray))
        }
        crate::PeerState::Connecting(address) => {
            Row::new(vec![
                Cell::from(DOT), 
                attributes,
                Cell::from(match peer.display_name.as_ref() {
                    Some(name) => name.clone(),
                    None => peer.id.clone(),
                }).style(style),
                Cell::from(format!("@{}", address)).style(Style::default().fg(Color::DarkGray)),
            ]).style(Style::default().fg(CONNECTING))
        }
    }
}

fn render_peer_list<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect) {
    let rows: Vec<Row> = app.peers
        .values()
        .enumerate()
        .map(|(i, peer)| peer_row(peer, i == app.peer_index))
        .collect();
    let widget = Table::new(rows)
        .style(Style::default().fg(Color::White))
        .widths(&[Constraint::Length(1), Constraint::Length(2), Constraint::Min(70), Constraint::Min(24)])
        .column_spacing(1)
        .block(default_block());
    f.render_widget(widget, area);
}

fn render_editor<'a>(editor: &'a Editor) -> Paragraph<'a> {
    let before_cursor: String = editor.buffer.chars().take(editor.cursor).collect();
    let at_cursor: String = editor
        .buffer
        .chars().nth(editor.cursor)
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

fn render_chat<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(area);
    let widget = render_editor(&app.editor).block(default_block());
    f.render_widget(widget, chunks[0]);
}

fn render_settings<B: Backend>(f: &mut Frame<B>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(area);
    let widget = Paragraph::new(vec![Spans::from(
        match app.own_address.as_ref() {
            Some(addr) => Spans::from(vec![
                Span::styled("Your address: ", Style::default().fg(Color::DarkGray)),
                Span::styled(addr.to_string(), Style::default().fg(Color::LightBlue)),
            ]),
            None => Spans::from(vec![Span::styled("Waiting for tor...".to_string(), Style::default().fg(Color::DarkGray))]),
        }
    )])
        .block(default_block())
        .style(Style::default().fg(Color::White));
    f.render_widget(widget, chunks[0]);
}