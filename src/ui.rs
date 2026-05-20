use std::collections::HashMap;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::sync::mpsc;

use crate::protocol::{ChatMessage, ClientMsg, ServerMsg};

struct App {
    nick: String,
    rooms: Vec<String>,
    current_room: String,
    messages: HashMap<String, Vec<ChatMessage>>,
    input: String,
    status: Option<String>,
}

impl App {
    fn new(nick: String) -> Self {
        App {
            nick,
            rooms: vec!["general".to_string()],
            current_room: "general".to_string(),
            messages: HashMap::new(),
            input: String::new(),
            status: None,
        }
    }

    fn push_msg(&mut self, room: &str, msg: ChatMessage) {
        self.messages.entry(room.to_string()).or_default().push(msg);
    }

    fn push_sys(&mut self, room: String, text: String) {
        self.push_msg(
            &room,
            ChatMessage {
                nick: String::new(),
                content: text,
                ts: chrono::Utc::now().timestamp(),
            },
        );
    }

    fn current_msgs(&self) -> &[ChatMessage] {
        self.messages
            .get(&self.current_room)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn update_rooms(&mut self, rooms: Vec<String>) {
        self.rooms = rooms;
        if !self.rooms.contains(&self.current_room) {
            self.rooms.push(self.current_room.clone());
            self.rooms.sort();
        }
    }
}

pub async fn run(
    nick: String,
    net_tx: mpsc::Sender<ClientMsg>,
    mut srv_rx: mpsc::Receiver<ServerMsg>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(nick);
    let mut events = EventStream::new();
    let mut quit = false;

    while !quit {
        terminal.draw(|f| draw(f, &app))?;

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(event)) => quit = handle_key(event, &mut app, &net_tx).await,
                    _ => quit = true,
                }
            }
            maybe_msg = srv_rx.recv() => {
                match maybe_msg {
                    Some(msg) => handle_srv(msg, &mut app),
                    None => {
                        app.status = Some("disconnected".into());
                        quit = true;
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

async fn handle_key(
    event: Event,
    app: &mut App,
    net_tx: &mpsc::Sender<ClientMsg>,
) -> bool {
    let Event::Key(key) = event else { return false };

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) | (_, KeyCode::Esc) => return true,

        (_, KeyCode::Enter) => {
            let content: String = app.input.drain(..).collect();
            if !content.is_empty() {
                if let Some(rest) = content.strip_prefix('/') {
                    handle_cmd(rest, app, net_tx).await;
                } else {
                    let _ = net_tx.send(ClientMsg::Send { content }).await;
                }
            }
        }

        (_, KeyCode::Backspace) => {
            app.input.pop();
        }

        (_, KeyCode::Char(c)) => app.input.push(c),

        (KeyModifiers::ALT, KeyCode::Up) => {
            if let Some(idx) = room_idx(app) {
                if idx > 0 {
                    let room = app.rooms[idx - 1].clone();
                    do_switch(app, net_tx, room).await;
                }
            }
        }

        (KeyModifiers::ALT, KeyCode::Down) => {
            if let Some(idx) = room_idx(app) {
                let next = idx + 1;
                if next < app.rooms.len() {
                    let room = app.rooms[next].clone();
                    do_switch(app, net_tx, room).await;
                }
            }
        }

        _ => {}
    }

    false
}

fn room_idx(app: &App) -> Option<usize> {
    app.rooms.iter().position(|r| r == &app.current_room)
}

async fn do_switch(app: &mut App, net_tx: &mpsc::Sender<ClientMsg>, room: String) {
    let _ = net_tx.send(ClientMsg::SwitchRoom { room: room.clone() }).await;
    app.current_room = room;
}

async fn handle_cmd(cmd: &str, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) {
    let mut parts = cmd.splitn(2, ' ');
    match parts.next().unwrap_or("") {
        "join" | "j" => {
            if let Some(room) = parts.next() {
                do_switch(app, net_tx, room.to_string()).await;
            }
        }
        "create" | "new" => {
            if let Some(name) = parts.next() {
                let _ = net_tx.send(ClientMsg::CreateRoom { name: name.to_string() }).await;
            }
        }
        "rooms" | "list" => {
            let _ = net_tx.send(ClientMsg::ListRooms).await;
        }
        other => {
            let room = app.current_room.clone();
            app.push_sys(room, format!("unknown command: /{other}"));
        }
    }
}

fn handle_srv(msg: ServerMsg, app: &mut App) {
    match msg {
        ServerMsg::Joined { room, history } => {
            app.current_room = room.clone();
            *app.messages.entry(room.clone()).or_default() = history;
            if !app.rooms.contains(&room) {
                app.rooms.push(room);
                app.rooms.sort();
            }
        }

        ServerMsg::SwitchedRoom { room, history } => {
            app.current_room = room.clone();
            *app.messages.entry(room.clone()).or_default() = history;
            if !app.rooms.contains(&room) {
                app.rooms.push(room);
                app.rooms.sort();
            }
        }

        ServerMsg::Message { room, nick, content, ts } => {
            app.push_msg(&room, ChatMessage { nick, content, ts });
        }

        ServerMsg::UserJoined { room, nick } => {
            app.push_sys(room, format!("-> {nick} joined"));
        }

        ServerMsg::UserLeft { room, nick } => {
            app.push_sys(room, format!("<- {nick} left"));
        }

        ServerMsg::RoomList { rooms } => {
            app.update_rooms(rooms);
        }

        ServerMsg::Error { msg } => {
            app.status = Some(format!("error: {msg}"));
        }

        ServerMsg::Pong => {}
    }
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    let [body, input_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);

    let [sidebar, messages_area] =
        Layout::horizontal([Constraint::Length(22), Constraint::Min(1)]).areas(body);

    draw_rooms(f, app, sidebar);
    draw_messages(f, app, messages_area);
    draw_input(f, app, input_area);
}

fn draw_rooms(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .rooms
        .iter()
        .map(|room| {
            if room == &app.current_room {
                ListItem::new(format!("  #{room}"))
                    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            } else {
                ListItem::new(format!("  #{room}"))
                    .style(Style::default().fg(Color::DarkGray))
            }
        })
        .collect();

    let block = Block::default()
        .title(Span::styled(
            " dusk ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    f.render_widget(List::new(items).block(block), area);
}

fn draw_messages(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let title = format!(" #{} ", app.current_room);

    let block = Block::default()
        .title(Span::styled(
            title.clone(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner_height = block.inner(area).height as usize;
    let msgs = app.current_msgs();
    let start = msgs.len().saturating_sub(inner_height);

    let lines: Vec<Line> = msgs[start..]
        .iter()
        .map(|m| {
            if m.nick.is_empty() {
                Line::from(Span::styled(
                    format!("  {}", m.content),
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                use chrono::TimeZone;
                let ts = chrono::Utc
                    .timestamp_opt(m.ts, 0)
                    .single()
                    .map(|dt| dt.format("%H:%M").to_string())
                    .unwrap_or_else(|| "--:--".to_string());
                Line::from(vec![
                    Span::styled(
                        format!("{ts} "),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{}: ", m.nick),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(m.content.clone()),
                ])
            }
        })
        .collect();

    let para = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(para, area);
}

fn draw_input(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let title = match &app.status {
        Some(s) => format!(" {s} "),
        None => format!(" {} ", app.nick),
    };

    let block = Block::default()
        .title(Span::styled(
            title.clone(),
            Style::default().fg(Color::Cyan),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let display = format!("> {}", app.input);
    f.render_widget(Paragraph::new(display.as_str()).block(block), area);

    let cx = area.x + 1 + 2 + app.input.len() as u16;
    let cy = area.y + 1;
    let max_x = area.x + area.width.saturating_sub(2);
    if cx <= max_x {
        f.set_cursor_position((cx, cy));
    }
}
