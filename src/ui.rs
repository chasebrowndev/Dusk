use std::collections::HashMap;

use anyhow::Result;
use chrono::{Local, TimeZone};
use crossterm::{
    event::{
        Event, EventStream, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use tokio::sync::mpsc;

use base64::{engine::general_purpose::STANDARD, Engine};

use crate::config::Config;
use crate::protocol::{ChatMessage, ClientMsg, ServerMsg};
use crate::theme::{Theme, by_name};
use crate::voice::VoiceHandle;

use ratatui::widgets::Clear;

const SPLASH_ART: &[&str] = &[
    "·▄▄▄▄  ▄• ▄▌.▄▄ · ▄ •▄ ",
    "██▪ ██ █▪██▌▐█ ▀. █▌▄▌▪",
    "▐█· ▐█▌█▌▐█▌▄▀▀▀█▄▐▀▀▄·",
    "██. ██ ▐█▄█▌▐█▄▪▐█▐█.█▌",
    "▀▀▀▀▀•  ▀▀▀  ▀▀▀▀ ·▀  ▀",
];

// Screen/camera sharing: Dusk stays text-only and just signals peers, then
// hands off to external capture/playback tools. The commands below are
// overridable via the DUSK_SHARE_* environment variables; `{addr}` is
// substituted with the sharer's `host:port`.
const SHARE_PORT: u16 = 7668;
const DEFAULT_SHARE_SCREEN: &str =
    "wf-recorder -c libx264 -x yuv420p -F mpegts -f - 2>/dev/null | ffmpeg -loglevel quiet -i pipe: -c copy -f mpegts -listen 1 tcp://{addr}";
const DEFAULT_SHARE_CAM: &str =
    "ffmpeg -loglevel quiet -f v4l2 -i /dev/video0 -vcodec libx264 -preset ultrafast -tune zerolatency -f mpegts -listen 1 tcp://{addr}";
const DEFAULT_SHARE_VIEW: &str = "ffplay -loglevel quiet -fflags nobuffer -flags low_delay -i tcp://{addr}";

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Rooms,
    Input,
    Settings,
}

struct SettingsState {
    cursor: usize,
    audio_inputs: Vec<String>,
    audio_outputs: Vec<String>,
    video_devices: Vec<String>,
    themes: Vec<String>,
    audio_in_idx: usize,
    audio_out_idx: usize,
    video_idx: usize,
    theme_idx: usize,
}

impl SettingsState {
    const NUM_ROWS: usize = 4;

    fn new(cfg_audio_in: Option<&str>, cfg_audio_out: Option<&str>, cfg_video: Option<&str>, current_theme: &str) -> Self {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();

        let mut audio_inputs: Vec<String> = host
            .input_devices()
            .map(|d| d.filter_map(|dev| dev.name().ok()).collect())
            .unwrap_or_default();
        if audio_inputs.is_empty() {
            audio_inputs.push("(none)".into());
        }

        let mut audio_outputs: Vec<String> = host
            .output_devices()
            .map(|d| d.filter_map(|dev| dev.name().ok()).collect())
            .unwrap_or_default();
        if audio_outputs.is_empty() {
            audio_outputs.push("(none)".into());
        }

        let mut video_devices: Vec<String> = (0..8)
            .map(|i| format!("/dev/video{i}"))
            .filter(|p| std::path::Path::new(p).exists())
            .collect();
        if video_devices.is_empty() {
            video_devices.push("(none)".into());
        }

        let themes: Vec<String> = crate::theme::ALL.iter().map(|s| s.to_string()).collect();

        let audio_in_idx = cfg_audio_in
            .and_then(|n| audio_inputs.iter().position(|x| x == n))
            .unwrap_or(0);
        let audio_out_idx = cfg_audio_out
            .and_then(|n| audio_outputs.iter().position(|x| x == n))
            .unwrap_or(0);
        let video_idx = cfg_video
            .and_then(|n| video_devices.iter().position(|x| x == n))
            .unwrap_or(0);
        let theme_idx = themes.iter().position(|t| t == current_theme).unwrap_or(0);

        SettingsState {
            cursor: 0,
            audio_inputs,
            audio_outputs,
            video_devices,
            themes,
            audio_in_idx,
            audio_out_idx,
            video_idx,
            theme_idx,
        }
    }

    fn audio_in(&self) -> &str {
        self.audio_inputs.get(self.audio_in_idx).map(String::as_str).unwrap_or("(none)")
    }

    fn audio_out(&self) -> &str {
        self.audio_outputs.get(self.audio_out_idx).map(String::as_str).unwrap_or("(none)")
    }

    fn video(&self) -> &str {
        self.video_devices.get(self.video_idx).map(String::as_str).unwrap_or("(none)")
    }

    fn theme_name(&self) -> &str {
        self.themes.get(self.theme_idx).map(String::as_str).unwrap_or("cyberpunk")
    }
}

struct App {
    nick: String,
    rooms: Vec<String>,
    current_room: String,
    messages: HashMap<String, Vec<ChatMessage>>,
    input: String,
    cursor: usize, // byte offset into `input`, kept on a char boundary
    status: Option<String>,
    focus: Focus,
    room_cursor: usize,
    scroll: HashMap<String, usize>,
    theme: &'static Theme,
    voice: Option<VoiceHandle>,
    voice_users: HashMap<String, Vec<String>>, // room -> nicks in voice
    room_users: HashMap<String, Vec<String>>,  // room -> nicks present
    share_child: Option<std::process::Child>,  // local capture process, if sharing
    shares: HashMap<String, String>,           // nick -> stream addr of active shares
    msg_width: u16,
    msg_height: u16,
    settings: SettingsState,
    audio_in: Option<String>,
    audio_out: Option<String>,
    video_device: Option<String>,
}

impl App {
    fn new(nick: String, theme: &'static Theme) -> Self {
        let cfg = Config::load().ok().flatten();
        let audio_in = cfg.as_ref().and_then(|c| c.audio_input.clone());
        let audio_out = cfg.as_ref().and_then(|c| c.audio_output.clone());
        let video_device = cfg.as_ref().and_then(|c| c.video_device.clone());
        let settings = SettingsState::new(
            audio_in.as_deref(),
            audio_out.as_deref(),
            video_device.as_deref(),
            theme.name,
        );
        App {
            nick,
            rooms: vec!["general".to_string()],
            current_room: "general".to_string(),
            messages: HashMap::new(),
            input: String::new(),
            cursor: 0,
            status: None,
            focus: Focus::Input,
            room_cursor: 0,
            scroll: HashMap::new(),
            theme,
            voice: None,
            voice_users: HashMap::new(),
            room_users: HashMap::new(),
            share_child: None,
            shares: HashMap::new(),
            msg_width: 0,
            msg_height: 0,
            settings,
            audio_in,
            audio_out,
            video_device,
        }
    }

    fn push_msg(&mut self, room: &str, msg: ChatMessage) {
        // If scrolled back in this room, advance the offset by the new
        // message's wrapped line count so the viewport stays anchored.
        if room == self.current_room && self.current_scroll() > 0 && self.msg_width > 0 {
            let added = message_lines(&msg, self.msg_width as usize, self.theme).len();
            let offset = self.current_scroll();
            self.scroll.insert(room.to_string(), offset + added);
        }
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

    fn current_scroll(&self) -> usize {
        *self.scroll.get(&self.current_room).unwrap_or(&0)
    }

    // Scroll offset is measured in wrapped display lines from the bottom.
    // The upper bound is clamped in draw_messages, where the viewport height
    // and total line count are known.
    fn scroll_up(&mut self, n: usize) {
        let current = self.current_scroll();
        self.scroll.insert(self.current_room.clone(), current + n);
    }

    fn scroll_down(&mut self, n: usize) {
        let current = self.current_scroll();
        self.scroll.insert(self.current_room.clone(), current.saturating_sub(n));
    }

    fn update_rooms(&mut self, rooms: Vec<String>) {
        self.rooms = rooms;
        if !self.rooms.contains(&self.current_room) {
            self.rooms.push(self.current_room.clone());
            self.rooms.sort();
        }
        self.room_cursor = self.room_cursor.min(self.rooms.len().saturating_sub(1));
    }
}

pub async fn run(
    nick: String,
    theme_name: String,
    net_tx: mpsc::Sender<ClientMsg>,
    mut srv_rx: mpsc::Receiver<ServerMsg>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Enable the enhanced keyboard protocol so Shift+Enter is reported
    // distinctly from Enter. Terminals without support are left untouched.
    let kbd_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if kbd_enhanced {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let theme = by_name(&theme_name);
    let mut events = EventStream::new();

    // Splash — draw once and hold for 1.8 s, then enter the main loop.
    terminal.draw(|f| draw_splash(f, theme))?;
    tokio::time::sleep(std::time::Duration::from_millis(1800)).await;

    let mut app = App::new(nick, theme);
    let mut quit = false;

    while !quit {
        terminal.draw(|f| draw(f, &mut app))?;

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

    if let Some(mut child) = app.share_child.take() {
        kill_share(&mut child);
    }
    if kbd_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

async fn handle_key(event: Event, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    let Event::Key(key) = event else { return false };
    match app.focus {
        Focus::Input => handle_input_key(key, app, net_tx).await,
        Focus::Rooms => handle_rooms_key(key, app, net_tx).await,
        Focus::Settings => handle_settings_key(key, app, net_tx).await,
    }
}

async fn handle_input_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, Char('c')) => return true,

        // Tab: move focus to room sidebar, sync cursor to current room
        (_, Tab) => {
            app.focus = Focus::Rooms;
            app.room_cursor = app
                .rooms
                .iter()
                .position(|r| r == &app.current_room)
                .unwrap_or(0);
        }

        // Shift+Enter / Alt+Enter: insert a newline (Alt is the fallback for
        // terminals without the enhanced keyboard protocol).
        (KeyModifiers::SHIFT, Enter) | (KeyModifiers::ALT, Enter) => {
            app.input.insert(app.cursor, '\n');
            app.cursor += 1;
        }

        // Enter: send the message / run the command
        (_, Enter) => {
            let content: String = app.input.drain(..).collect();
            app.cursor = 0;
            if !content.trim().is_empty() {
                if let Some(rest) = content.strip_prefix('/') {
                    handle_cmd(rest, app, net_tx).await;
                } else {
                    let _ = net_tx.send(ClientMsg::Send { content }).await;
                    app.scroll.insert(app.current_room.clone(), 0);
                }
            }
        }

        // Backspace: delete the char before the cursor
        (_, Backspace) => {
            if app.cursor > 0 {
                let prev = prev_char_boundary(&app.input, app.cursor);
                app.input.replace_range(prev..app.cursor, "");
                app.cursor = prev;
            }
        }

        // Ctrl+W: delete the word before the cursor
        (KeyModifiers::CONTROL, Char('w')) => {
            let head = &app.input[..app.cursor];
            let trimmed_len = head.trim_end_matches([' ', '\n']).len();
            let cut = head[..trimmed_len]
                .rfind([' ', '\n'])
                .map(|i| i + 1)
                .unwrap_or(0);
            app.input.replace_range(cut..app.cursor, "");
            app.cursor = cut;
        }

        // Ctrl+U: clear the input
        (KeyModifiers::CONTROL, Char('u')) => {
            app.input.clear();
            app.cursor = 0;
        }

        // V: toggle voice (only when there is nothing to send)
        (KeyModifiers::NONE, Char('v')) if app.input.is_empty() => {
            toggle_voice(app, net_tx).await;
        }

        // Printable char: insert at the cursor
        (_, Char(c)) => {
            app.input.insert(app.cursor, c);
            app.cursor += c.len_utf8();
        }

        // Left/Right: move the cursor through the typed text
        (_, Left) => app.cursor = prev_char_boundary(&app.input, app.cursor),
        (_, Right) => app.cursor = next_char_boundary(&app.input, app.cursor),

        // Up/Down: move the cursor between lines of the typed text
        (_, Up) => app.cursor = move_cursor_vertical(&app.input, app.cursor, -1),
        (_, Down) => app.cursor = move_cursor_vertical(&app.input, app.cursor, 1),

        // Home/End: jump to start/end of the input
        (_, Home) => app.cursor = 0,
        (_, End) => app.cursor = app.input.len(),

        // PageUp/PageDown: smooth scroll through message history
        (_, PageUp) => {
            let step = (app.msg_height as usize / 2).max(1);
            app.scroll_up(step);
        }
        (_, PageDown) => {
            let step = (app.msg_height as usize / 2).max(1);
            app.scroll_down(step);
        }

        _ => {}
    }
    false
}

async fn handle_rooms_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, Char('c')) => return true,

        (_, Esc) => {
            app.focus = Focus::Input;
        }

        (_, Tab) => {
            // Re-enumerate devices in case they changed since last open
            app.settings = SettingsState::new(
                app.audio_in.as_deref(),
                app.audio_out.as_deref(),
                app.video_device.as_deref(),
                app.theme.name,
            );
            app.focus = Focus::Settings;
        }

        (_, Up) => {
            if app.rooms.is_empty() {
                return false;
            }
            app.room_cursor = if app.room_cursor == 0 {
                app.rooms.len() - 1
            } else {
                app.room_cursor - 1
            };
        }

        (_, Down) => {
            if !app.rooms.is_empty() {
                app.room_cursor = (app.room_cursor + 1) % app.rooms.len();
            }
        }

        (_, Enter) => {
            if let Some(room) = app.rooms.get(app.room_cursor).cloned() {
                do_switch(app, net_tx, room).await;
            }
            app.focus = Focus::Input;
        }

        _ => {}
    }
    false
}

async fn handle_settings_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, Char('c')) => return true,

        (_, Esc) | (_, Tab) => {
            apply_settings(app).await;
            app.focus = Focus::Input;
        }

        (_, Up) => {
            if app.settings.cursor > 0 {
                app.settings.cursor -= 1;
            }
        }

        (_, Down) => {
            if app.settings.cursor + 1 < SettingsState::NUM_ROWS {
                app.settings.cursor += 1;
            }
        }

        (_, Left) => cycle_setting(app, -1, net_tx).await,
        (_, Right) => cycle_setting(app, 1, net_tx).await,

        _ => {}
    }
    false
}

async fn cycle_setting(app: &mut App, dir: i32, _net_tx: &mpsc::Sender<ClientMsg>) {
    let s = &mut app.settings;
    match s.cursor {
        0 => {
            let len = s.audio_inputs.len();
            s.audio_in_idx = cycle_idx(s.audio_in_idx, len, dir);
        }
        1 => {
            let len = s.audio_outputs.len();
            s.audio_out_idx = cycle_idx(s.audio_out_idx, len, dir);
        }
        2 => {
            let len = s.video_devices.len();
            s.video_idx = cycle_idx(s.video_idx, len, dir);
        }
        3 => {
            let len = s.themes.len();
            s.theme_idx = cycle_idx(s.theme_idx, len, dir);
            // Live-preview theme change
            app.theme = by_name(app.settings.theme_name());
        }
        _ => {}
    }
}

fn cycle_idx(idx: usize, len: usize, dir: i32) -> usize {
    if len == 0 {
        return 0;
    }
    if dir > 0 {
        (idx + 1) % len
    } else if idx == 0 {
        len - 1
    } else {
        idx - 1
    }
}

async fn apply_settings(app: &mut App) {
    let audio_in = app.settings.audio_in().to_string();
    let audio_out = app.settings.audio_out().to_string();
    let video = app.settings.video().to_string();
    let theme_name = app.settings.theme_name().to_string();

    app.audio_in = if audio_in == "(none)" { None } else { Some(audio_in.clone()) };
    app.audio_out = if audio_out == "(none)" { None } else { Some(audio_out.clone()) };
    app.video_device = if video == "(none)" { None } else { Some(video.clone()) };
    app.theme = by_name(&theme_name);

    let _ = Config::update_devices(
        app.audio_in.as_deref(),
        app.audio_out.as_deref(),
        app.video_device.as_deref(),
        &theme_name,
    );
}

async fn toggle_voice(app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) {
    if app.voice.is_some() {
        app.voice = None;
        let _ = net_tx.send(ClientMsg::VoiceLeave).await;
        let room = app.current_room.clone();
        app.push_sys(room, "left voice".into());
    } else {
        match crate::voice::start(net_tx.clone(), app.audio_in.as_deref(), app.audio_out.as_deref()) {
            Ok(handle) => {
                app.voice = Some(handle);
                let _ = net_tx.send(ClientMsg::VoiceJoin).await;
                let room = app.current_room.clone();
                app.push_sys(room, "joined voice — mic active".into());
            }
            Err(e) => {
                let room = app.current_room.clone();
                app.push_sys(room, format!("voice error: {e}"));
            }
        }
    }
}

async fn do_switch(app: &mut App, net_tx: &mpsc::Sender<ClientMsg>, room: String) {
    if room != app.current_room {
        let _ = net_tx.send(ClientMsg::SwitchRoom { room: room.clone() }).await;
        app.current_room = room;
    }
}

// Byte offset of the char boundary just before `idx`.
fn prev_char_boundary(s: &str, idx: usize) -> usize {
    s[..idx].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
}

// Byte offset of the char boundary just after `idx`.
fn next_char_boundary(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map(|c| idx + c.len_utf8()).unwrap_or(idx)
}

// Row (0-based line) and column (0-based char) of `cursor` within `input`.
fn cursor_rowcol(input: &str, cursor: usize) -> (usize, usize) {
    let before = &input[..cursor];
    let row = before.bytes().filter(|&b| b == b'\n').count();
    let col = before.rsplit('\n').next().unwrap_or("").chars().count();
    (row, col)
}

// Move the cursor up (dir = -1) or down (dir = 1) one line, keeping the column.
fn move_cursor_vertical(input: &str, cursor: usize, dir: i32) -> usize {
    let (row, col) = cursor_rowcol(input, cursor);
    let lines: Vec<&str> = input.split('\n').collect();
    let target = row as i32 + dir;
    if target < 0 {
        return 0;
    }
    if target as usize >= lines.len() {
        return input.len();
    }
    let target = target as usize;
    let mut offset = 0;
    for l in &lines[..target] {
        offset += l.len() + 1;
    }
    let line = lines[target];
    let c = col.min(line.chars().count());
    offset + line.char_indices().nth(c).map(|(i, _)| i).unwrap_or(line.len())
}

async fn handle_cmd(cmd: &str, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) {
    let mut parts = cmd.splitn(2, ' ');
    let verb = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim);

    match verb {
        "join" | "j" => {
            if let Some(room) = arg {
                do_switch(app, net_tx, room.to_string()).await;
            }
        }
        "create" | "new" => {
            if let Some(name) = arg {
                let _ = net_tx.send(ClientMsg::CreateRoom { name: name.to_string() }).await;
            }
        }
        "rooms" | "list" => {
            let _ = net_tx.send(ClientMsg::ListRooms).await;
        }
        "theme" => {
            if let Some(name) = arg {
                app.theme = by_name(name);
                let _ = Config::update_theme(name);
                let room = app.current_room.clone();
                app.push_sys(room, format!("theme → {}", app.theme.name));
            } else {
                let room = app.current_room.clone();
                app.push_sys(
                    room,
                    format!(
                        "themes: {}  (current: {})",
                        crate::theme::ALL.join("  "),
                        app.theme.name
                    ),
                );
            }
        }
        "share" => {
            share_cmd(app, net_tx, arg).await;
        }
        "watch" => {
            watch_cmd(app, arg);
        }
        "help" | "?" => {
            let room = app.current_room.clone();
            for line in [
                "commands: /join <room>  /create <room>  /rooms  /theme [name]  /help",
                "sharing: /share [cam]  /share stop  /watch [nick]",
                "keys: tab=sidebar  pgup/pgdn=scroll  ctrl+c=quit",
                "input: ←→↑↓=move cursor  shift+enter/alt+enter=newline",
                "input: ctrl+w=del word  ctrl+u=clear",
            ] {
                app.push_sys(room.clone(), line.into());
            }
        }
        _ => {
            let room = app.current_room.clone();
            app.push_sys(room, format!("unknown command /{verb} — try /help"));
        }
    }
}

// ---------------------------------------------------------------------------
// Screen / camera sharing (signal + external tool hand-off)
// ---------------------------------------------------------------------------

// The sharer's `host:port`, derived from this machine's Tailscale IP.
fn share_addr() -> Option<String> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()?;
    let ip = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    Some(format!("{ip}:{SHARE_PORT}"))
}

// Capture command template for the requested kind, env-overridable.
fn share_template(kind: &str, video_device: Option<&str>) -> String {
    match kind {
        "cam" | "camera" | "webcam" => {
            let tpl = std::env::var("DUSK_SHARE_CAM").unwrap_or_else(|_| DEFAULT_SHARE_CAM.into());
            if let Some(dev) = video_device {
                tpl.replace("/dev/video0", dev)
            } else {
                tpl
            }
        }
        _ => std::env::var("DUSK_SHARE_SCREEN").unwrap_or_else(|_| DEFAULT_SHARE_SCREEN.into()),
    }
}

// Spawn a shell command detached from the TUI: no shared stdio, and in its own
// process group so the whole pipeline can be signalled as a unit.
fn spawn_detached(cmd: &str) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

// Terminate the capture process and its whole group (a negative pid to `kill`
// signals the process group, catching piped children like ffmpeg).
fn kill_share(child: &mut std::process::Child) {
    let pid = child.id();
    let _ = std::process::Command::new("kill")
        .arg(format!("-{pid}"))
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

async fn share_cmd(app: &mut App, net_tx: &mpsc::Sender<ClientMsg>, arg: Option<&str>) {
    let room = app.current_room.clone();

    if arg == Some("stop") {
        match app.share_child.take() {
            Some(mut child) => {
                kill_share(&mut child);
                let _ = net_tx.send(ClientMsg::ShareStop).await;
                app.push_sys(room, "stopped sharing".into());
            }
            None => app.push_sys(room, "not currently sharing".into()),
        }
        return;
    }

    if app.share_child.is_some() {
        app.push_sys(room, "already sharing — /share stop first".into());
        return;
    }

    let Some(addr) = share_addr() else {
        app.push_sys(room, "share: could not detect Tailscale IP".into());
        return;
    };

    let kind = arg.unwrap_or("screen");
    let cmd = share_template(kind, app.video_device.as_deref()).replace("{addr}", &addr);
    match spawn_detached(&cmd) {
        Ok(child) => {
            app.share_child = Some(child);
            let _ = net_tx.send(ClientMsg::ShareStart { url: addr.clone() }).await;
            let nick = app.nick.clone();
            app.push_sys(room, format!("sharing {kind} at {addr} — peers: /watch {nick}"));
        }
        Err(e) => app.push_sys(room, format!("share error: {e}")),
    }
}

fn watch_cmd(app: &mut App, arg: Option<&str>) {
    let room = app.current_room.clone();

    let url = match arg {
        Some(nick) => app.shares.get(nick).cloned(),
        None if app.shares.len() == 1 => app.shares.values().next().cloned(),
        None => None,
    };

    let Some(url) = url else {
        if app.shares.is_empty() {
            app.push_sys(room, "no active shares".into());
        } else {
            let who: Vec<&str> = app.shares.keys().map(String::as_str).collect();
            app.push_sys(room, format!("/watch <nick> — sharing now: {}", who.join(" ")));
        }
        return;
    };

    let view = std::env::var("DUSK_SHARE_VIEW").unwrap_or_else(|_| DEFAULT_SHARE_VIEW.into());
    let cmd = view.replace("{addr}", &url);
    match spawn_detached(&cmd) {
        Ok(_) => app.push_sys(room, format!("opening stream {url}")),
        Err(e) => app.push_sys(room, format!("watch error: {e}")),
    }
}

fn handle_srv(msg: ServerMsg, app: &mut App) {
    match msg {
        ServerMsg::Joined { room, history, users } => {
            app.current_room = room.clone();
            *app.messages.entry(room.clone()).or_default() = history;
            app.room_users.insert(room.clone(), users);
            if !app.rooms.contains(&room) {
                app.rooms.push(room);
                app.rooms.sort();
            }
            app.scroll.insert(app.current_room.clone(), 0);
        }

        ServerMsg::SwitchedRoom { room, history, users } => {
            app.current_room = room.clone();
            *app.messages.entry(room.clone()).or_default() = history;
            app.room_users.insert(room.clone(), users);
            if !app.rooms.contains(&room) {
                app.rooms.push(room);
                app.rooms.sort();
            }
            app.scroll.insert(app.current_room.clone(), 0);
        }

        ServerMsg::Message { room, nick, content, ts } => {
            app.push_msg(&room, ChatMessage { nick, content, ts });
        }

        ServerMsg::UserJoined { room, nick, users } => {
            app.room_users.insert(room.clone(), users);
            app.push_sys(room, format!("→ {nick} joined"));
        }

        ServerMsg::UserLeft { room, nick, users } => {
            app.room_users.insert(room.clone(), users);
            app.push_sys(room, format!("← {nick} left"));
        }

        ServerMsg::RoomList { rooms } => {
            app.update_rooms(rooms);
        }

        ServerMsg::VoiceJoined { room, nick, users } => {
            app.voice_users.insert(room.clone(), users);
            if nick != app.nick {
                app.push_sys(room, format!("mic {nick} joined voice"));
            }
        }

        ServerMsg::VoiceLeft { room, nick, users } => {
            app.voice_users.insert(room.clone(), users);
            if nick != app.nick {
                app.push_sys(room, format!("mic {nick} left voice"));
            }
        }

        ServerMsg::VoiceFrame { data, .. } => {
            if let Some(ref v) = app.voice {
                if let Ok(bytes) = STANDARD.decode(&data) {
                    let _ = v.frame_in.try_send(bytes);
                }
            }
        }

        ServerMsg::ShareStarted { room, nick, url } => {
            if nick != app.nick {
                app.shares.insert(nick.clone(), url);
                app.push_sys(room, format!("{nick} is sharing — /watch {nick} to view"));
            }
        }

        ServerMsg::ShareStopped { room, nick } => {
            if nick != app.nick {
                app.shares.remove(&nick);
                app.push_sys(room, format!("{nick} stopped sharing"));
            }
        }

        ServerMsg::Error { msg } => {
            app.status = Some(format!("error: {msg}"));
        }

        ServerMsg::Pong => {}
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Grow the input box with the typed text, up to 6 lines.
    let input_h = app.input.split('\n').count().clamp(1, 6) as u16 + 2;

    let [active_area, body, input_area, hints_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .areas(area);

    let [sidebar, messages_area] =
        Layout::horizontal([Constraint::Length(22), Constraint::Min(1)]).areas(body);

    draw_active_bar(f, app, active_area);
    draw_rooms(f, app, sidebar);
    draw_messages(f, app, messages_area);
    draw_input(f, app, input_area);
    draw_hints(f, app, hints_area);

    if app.focus == Focus::Settings {
        draw_settings(f, app, area);
    }
}

fn draw_rooms(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let focused = app.focus == Focus::Rooms;
    let settings_focused = app.focus == Focus::Settings;

    // Split: room list on top, settings button on bottom (3 lines for a bordered block)
    let [list_area, btn_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(area);

    let items: Vec<ListItem> = app
        .rooms
        .iter()
        .enumerate()
        .map(|(i, room)| {
            let is_current = room == &app.current_room;
            let is_cursor = focused && i == app.room_cursor;
            let prefix = if is_cursor { "> " } else { "  " };
            let text = format!("{prefix}#{room}");
            let style = match (is_current, is_cursor) {
                (true, true) => Style::default()
                    .fg(t.room_active)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                (true, false) => Style::default().fg(t.room_active).add_modifier(Modifier::BOLD),
                (false, true) => Style::default().fg(t.room_active),
                (false, false) => Style::default().fg(t.room_inactive),
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let border_style = if focused {
        Style::default().fg(t.border_focus)
    } else {
        Style::default().fg(t.border_dim)
    };

    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", app.nick),
            Style::default().fg(t.room_active).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(border_style);

    f.render_widget(List::new(items).block(block), list_area);

    // Settings button
    let btn_style = if settings_focused {
        Style::default().fg(t.room_active).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(t.room_inactive)
    };
    let btn_border = if settings_focused {
        Style::default().fg(t.border_focus)
    } else {
        Style::default().fg(t.border_dim)
    };
    let btn_block = Block::default()
        .borders(Borders::ALL)
        .border_style(btn_border);
    let btn_text = Paragraph::new(Span::styled(" ⚙ settings", btn_style)).block(btn_block);
    f.render_widget(btn_text, btn_area);
}

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    // Borders::ALL trims one cell on each side.
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    app.msg_width = inner_w as u16;
    app.msg_height = inner_h as u16;

    // Wrap every message into display lines up front so scrolling is exact.
    let lines: Vec<Line> = app
        .current_msgs()
        .iter()
        .flat_map(|m| message_lines(m, inner_w, t))
        .collect();

    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    let raw = app.current_scroll();
    let scroll = raw.min(max_scroll);
    if scroll != raw {
        app.scroll.insert(app.current_room.clone(), scroll);
    }

    let start = max_scroll - scroll;
    let end = (start + inner_h).min(total);
    let visible: Vec<Line> = lines[start..end].to_vec();

    let scroll_label = if scroll > 0 { format!(" ↑{scroll}") } else { String::new() };
    let voice_label = app
        .voice_users
        .get(&app.current_room)
        .filter(|u| !u.is_empty())
        .map(|u| format!(" [mic {}] ", u.join(" ")))
        .unwrap_or_default();
    let title = format!(" #{}{}{} ", app.current_room, voice_label, scroll_label);

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(t.border_focus).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.border_dim));

    f.render_widget(Paragraph::new(Text::from(visible)).block(block), area);
}

fn draw_active_bar(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let users = app
        .room_users
        .get(&app.current_room)
        .cloned()
        .unwrap_or_default();
    let voice = app
        .voice_users
        .get(&app.current_room)
        .cloned()
        .unwrap_or_default();

    let mut spans = vec![Span::styled(
        " active: ",
        Style::default().fg(t.hint_text).add_modifier(Modifier::BOLD),
    )];

    if users.is_empty() {
        spans.push(Span::styled("(nobody)", Style::default().fg(t.room_inactive)));
    } else {
        for (i, u) in users.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let mut label = u.clone();
            if voice.contains(u) {
                label.push_str(" ♪");
            }
            if app.shares.contains_key(u) {
                label.push_str(" ▣");
            }
            let mut style = Style::default().fg(t.msg_nick);
            if u == &app.nick {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(label, style));
        }
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// Wrap a message into styled display lines that each fit within `width`
// columns. System messages (empty nick) are indented; chat messages keep a
// `HH:MM nick:` header on the first line and indent continuation lines.
// Explicit newlines in the content start a fresh wrapped segment.
fn message_lines(m: &ChatMessage, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let width = width.max(1);
    if m.nick.is_empty() {
        let avail = width.saturating_sub(2).max(1);
        m.content
            .split('\n')
            .flat_map(|seg| wrap_words(seg, avail, avail))
            .map(|chunk| {
                Line::from(Span::styled(
                    format!("  {chunk}"),
                    Style::default().fg(t.msg_system),
                ))
            })
            .collect()
    } else {
        let ts = Local
            .timestamp_opt(m.ts, 0)
            .single()
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".to_string());
        let header_w = ts.chars().count() + 1 + m.nick.chars().count() + 2;
        let first_avail = width.saturating_sub(header_w).max(1);
        let cont_avail = width.saturating_sub(2).max(1);
        let mut chunks: Vec<String> = Vec::new();
        for (si, seg) in m.content.split('\n').enumerate() {
            let first = if si == 0 { first_avail } else { cont_avail };
            chunks.extend(wrap_words(seg, first, cont_avail));
        }
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                if i == 0 {
                    Line::from(vec![
                        Span::styled(format!("{ts} "), Style::default().fg(t.msg_timestamp)),
                        Span::styled(
                            format!("{}: ", m.nick),
                            Style::default().fg(t.msg_nick).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(chunk, Style::default().fg(t.msg_text)),
                    ])
                } else {
                    Line::from(Span::styled(
                        format!("  {chunk}"),
                        Style::default().fg(t.msg_text),
                    ))
                }
            })
            .collect()
    }
}

// Word-wrap `text` so the first line fits `first` columns and every following
// line fits `rest`. Words longer than the limit are hard-split.
fn wrap_words(text: &str, first: usize, rest: usize) -> Vec<String> {
    let first = first.max(1);
    let rest = rest.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    let limit = |idx: usize| if idx == 0 { first } else { rest };

    for word in text.split_whitespace() {
        let cap = limit(out.len());
        let wlen = word.chars().count();
        let sep = if line_len == 0 { 0 } else { 1 };

        if line_len + sep + wlen <= cap {
            if sep == 1 {
                line.push(' ');
                line_len += 1;
            }
            line.push_str(word);
            line_len += wlen;
            continue;
        }

        if line_len > 0 {
            out.push(std::mem::take(&mut line));
        }

        let mut chars: Vec<char> = word.chars().collect();
        loop {
            let cap = limit(out.len());
            if chars.len() <= cap {
                line_len = chars.len();
                line = chars.into_iter().collect();
                break;
            }
            let chunk: String = chars.drain(..cap).collect();
            out.push(chunk);
        }
    }

    if line_len > 0 || out.is_empty() {
        out.push(line);
    }
    out
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let focused = app.focus == Focus::Input;

    let title = match &app.status {
        Some(s) => format!(" {s} "),
        None => format!(" #{} ", app.current_room),
    };

    let border_style = if focused {
        Style::default().fg(t.border_focus)
    } else {
        Style::default().fg(t.border_dim)
    };

    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(t.border_focus)))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner_h = area.height.saturating_sub(2) as usize;
    let lines: Vec<&str> = app.input.split('\n').collect();
    let (cur_row, cur_col) = cursor_rowcol(&app.input, app.cursor);

    // Window the visible lines so the cursor's line is always shown.
    let start = cur_row.saturating_sub(inner_h.saturating_sub(1));
    let display: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(start)
        .take(inner_h.max(1))
        .map(|(i, l)| {
            let prefix = if i == 0 { "> " } else { "  " };
            Line::from(Span::styled(
                format!("{prefix}{l}"),
                Style::default().fg(t.msg_text),
            ))
        })
        .collect();

    f.render_widget(Paragraph::new(Text::from(display)).block(block), area);

    if focused && inner_h > 0 {
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: inner_h as u16,
        };
        let cx = inner.x + 2 + cur_col as u16;
        let cy = inner.y + (cur_row - start) as u16;
        if cx < inner.x + inner.width {
            f.set_cursor_position((cx, cy));
        }
    }
}

fn draw_splash(f: &mut Frame, theme: &Theme) {
    let area = f.area();
    let mut lines: Vec<Line> = SPLASH_ART
        .iter()
        .map(|&l| Line::from(Span::styled(l, Style::default().fg(theme.border_focus))))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "lightweight chat over tailscale",
        Style::default().fg(theme.hint_text),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press any key to continue",
        Style::default().fg(theme.hint_text),
    )));

    let total_h = lines.len() as u16;
    let y = area.y + area.height.saturating_sub(total_h) / 2;
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: total_h.min(area.height.saturating_sub(y - area.y)),
    };
    f.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        rect,
    );
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let s = &app.settings;

    // Centered popup: 64 wide, tall enough for all rows + padding
    let popup_w = 64u16.min(area.width);
    let popup_h = 14u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let rect = Rect { x, y, width: popup_w, height: popup_h };

    f.render_widget(Clear, rect);

    let outer = Block::default()
        .title(Span::styled(
            " Settings ",
            Style::default().fg(t.border_focus).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.border_focus));

    let inner = outer.inner(rect);
    f.render_widget(outer, rect);

    // Layout: content rows on top, hint line on bottom
    let [content_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    // Build display rows: (label, value, cursor_pos) where cursor_pos is Some(i) if row i maps to settings.cursor
    struct Row {
        label: &'static str,
        value: String,
        cursor_idx: Option<usize>,
    }

    let rows: Vec<Row> = vec![
        Row { label: "  AUDIO", value: String::new(), cursor_idx: None },
        Row { label: "    Microphone", value: s.audio_in().to_string(), cursor_idx: Some(0) },
        Row { label: "    Speaker", value: s.audio_out().to_string(), cursor_idx: Some(1) },
        Row { label: "", value: String::new(), cursor_idx: None },
        Row { label: "  VIDEO", value: String::new(), cursor_idx: None },
        Row { label: "    Camera", value: s.video().to_string(), cursor_idx: Some(2) },
        Row { label: "", value: String::new(), cursor_idx: None },
        Row { label: "  APPEARANCE", value: String::new(), cursor_idx: None },
        Row { label: "    Theme", value: s.theme_name().to_string(), cursor_idx: Some(3) },
    ];

    // inner width = popup_w - 2 borders; label=18, arrows=4 => value gets the rest
    let value_col_w = (popup_w.saturating_sub(2)).saturating_sub(22) as usize;

    let lines: Vec<Line> = rows.iter().map(|row| {
        if row.cursor_idx.is_none() {
            // Section header or spacer
            if row.label.is_empty() {
                Line::from("")
            } else {
                Line::from(Span::styled(
                    row.label,
                    Style::default().fg(t.hint_text).add_modifier(Modifier::BOLD),
                ))
            }
        } else {
            let is_active = row.cursor_idx == Some(s.cursor);
            let label_style = if is_active {
                Style::default().fg(t.room_active).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.msg_text)
            };
            let arrow_style = Style::default().fg(t.border_focus);
            let val_style = if is_active {
                Style::default().fg(t.border_focus).add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(t.msg_nick)
            };

            // Truncate value to fit, left-pad to fixed width for alignment
            let val = if row.value.chars().count() > value_col_w {
                let truncated: String = row.value.chars().take(value_col_w.saturating_sub(1)).collect();
                format!("{}…", truncated)
            } else {
                format!("{:<width$}", row.value, width = value_col_w)
            };

            Line::from(vec![
                Span::styled(format!("{:<18}", row.label), label_style),
                Span::styled(if is_active { "< " } else { "  " }, arrow_style),
                Span::styled(val, val_style),
                Span::styled(if is_active { " >" } else { "  " }, arrow_style),
            ])
        }
    }).collect();

    f.render_widget(Paragraph::new(Text::from(lines)), content_area);

    f.render_widget(
        Paragraph::new(Span::styled(
            " ↑↓ navigate   ← → change   tab / esc  save & close",
            Style::default().fg(t.hint_text),
        )),
        hint_area,
    );
}

fn draw_hints(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let voice_hint = if app.voice.is_some() { "  v:mic-off" } else { "  v:voice" };
    let text = match app.focus {
        Focus::Input => {
            format!(" tab:rooms  pgup/dn:scroll  shift+↵:newline  /help{voice_hint}  ^c:quit")
        }
        Focus::Rooms => " ↑↓:navigate  enter:join  esc:back  tab:settings".to_string(),
        Focus::Settings => " ↑↓:navigate  ←→:change  tab/esc:save & close".to_string(),
    };
    f.render_widget(
        Paragraph::new(text.as_str()).style(Style::default().fg(t.hint_text)),
        area,
    );
}
