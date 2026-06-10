mod audio_routing;
mod commands;
mod share_view;
mod text;

use share_view::ViewerHandle;

use std::collections::HashMap;

use anyhow::Result;
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
// overridable via the DUSK_SHARE_* environment variables; `{addr}` is the
// sharer's public Tailscale `host:port`, `{self_addr}` is the loopback the
// sharer's own inline viewer connects to.
//
// We use ffmpeg's `tee` muxer to fan one encoded mpegts stream to both
// endpoints. `use_fifo=1` gives each output its own buffer thread so a slow
// or absent reader can't stall the encoder; `onfail=ignore` lets one output
// die (peer disconnects) without tearing down the other (your self-view).
const SHARE_PORT: u16 = 7668;
// Captures the PipeWire sink monitor (desktop audio, no mic). Dusk's own
// voice-chat output is isolated by AudioIsolation before this runs, so
// viewers won't hear call audio echoed back. Override: DUSK_SHARE_SCREEN.
const DEFAULT_SHARE_SCREEN: &str =
    "wf-recorder {output_flag} -c libx264 -x yuv420p -F mpegts -f - | ffmpeg -loglevel warning -f pulse -i @DEFAULT_MONITOR@ -i pipe: -map 1:v -map 0:a -c:v copy -c:a aac -b:a 128k -f tee \"[f=mpegts:use_fifo=1:onfail=ignore]tcp://{addr}?listen=1|[f=mpegts:use_fifo=1:onfail=ignore]tcp://{self_addr}?listen=1\"";
// {audio_src} = configured audio-input device (mic). Cam shares are face-cam
// style; voice goes through Dusk's own voice chat, so we capture mic here.
const DEFAULT_SHARE_CAM: &str =
    "ffmpeg -loglevel warning -f pulse -i {audio_src} -f v4l2 -i /dev/video0 -map 0:a -map 1:v -c:v libx264 -preset ultrafast -tune zerolatency -c:a aac -b:a 128k -f tee \"[f=mpegts:use_fifo=1:onfail=ignore]tcp://{addr}?listen=1|[f=mpegts:use_fifo=1:onfail=ignore]tcp://{self_addr}?listen=1\"";
const DEFAULT_SHARE_VIEW: &str = "ffplay -loglevel quiet -fflags nobuffer -flags low_delay -i tcp://{addr}";

// Modal state machine (Phase 4).
//
// Compose  — default; keystrokes go to the composer.
// Select   — focus ring moves between panes via arrows; Enter enters Pane mode
//            (or returns to Compose if the ring is on the Composer pane).
// Pane     — arrows do what the focused pane defines; Esc returns to Select.
// Settings — centered overlay; Esc closes back to Compose.
#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Compose,
    Select,
    Pane,
    Settings,
}

// 2×2 pane grid:  Rooms     Center
//                 Composer  Controls
#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Rooms,
    Center,
    Composer,
    Controls,
}

#[derive(PartialEq, Clone, Copy)]
enum CenterTab {
    Messages,
    Stream,
}

#[derive(PartialEq, Clone, Copy)]
enum ControlButton {
    Voice,
    ShareScreen,
    ShareCam,
    Settings,
}

const CONTROL_BUTTONS: [ControlButton; 4] = [
    ControlButton::Voice,
    ControlButton::ShareScreen,
    ControlButton::ShareCam,
    ControlButton::Settings,
];

impl Pane {
    // Layout: left column is Rooms (top) over Controls (bottom). Right column
    // is Center (full body height). Composer spans the full input row below.
    fn move_h(self, dir: i32) -> Self {
        match (self, dir) {
            (Pane::Rooms, 1) => Pane::Center,
            (Pane::Controls, 1) => Pane::Center,
            (Pane::Center, -1) => Pane::Rooms,
            _ => self,
        }
    }
    fn move_v(self, dir: i32) -> Self {
        match (self, dir) {
            (Pane::Rooms, 1) => Pane::Controls,
            (Pane::Controls, -1) => Pane::Rooms,
            (Pane::Controls, 1) => Pane::Composer,
            (Pane::Center, 1) => Pane::Composer,
            (Pane::Composer, -1) => Pane::Center,
            _ => self,
        }
    }
}

#[derive(Default, Clone)]
struct DeviceConfig {
    audio_in: Option<String>,
    audio_out: Option<String>,
    video_device: Option<String>,
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

/// cpal on Linux exposes every ALSA PCM, including rate-converter and channel-mix
/// plugin stubs that aren't real endpoints. Picking one of these and trying to
/// open it later blows up voice with "audio output not found: lavrate" or hangs.
/// Keep names that ALSA reports but are known not to be usable out of the menu.
fn is_real_alsa_device(name: &str) -> bool {
    !matches!(
        name,
        "lavrate"
            | "samplerate"
            | "speexrate"
            | "speexrate_best"
            | "speexrate_medium"
            | "upmix"
            | "vdownmix"
            | "null"
            | "surround21"
            | "surround40"
            | "surround41"
            | "surround50"
            | "surround51"
            | "surround71"
    )
}

impl SettingsState {
    const NUM_ROWS: usize = 4;

    fn new(cfg_audio_in: Option<&str>, cfg_audio_out: Option<&str>, cfg_video: Option<&str>, current_theme: &str) -> Self {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();

        let mut audio_inputs: Vec<String> = host
            .input_devices()
            .map(|d| d.filter_map(|dev| dev.name().ok()).filter(|n| is_real_alsa_device(n)).collect())
            .unwrap_or_default();
        if audio_inputs.is_empty() {
            audio_inputs.push("(none)".into());
        }

        let mut audio_outputs: Vec<String> = host
            .output_devices()
            .map(|d| d.filter_map(|dev| dev.name().ok()).filter(|n| is_real_alsa_device(n)).collect())
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
    mode: Mode,
    pane: Pane,
    center_tab: CenterTab,
    controls_cursor: usize,
    room_cursor: usize,
    scroll: HashMap<String, usize>,
    theme: &'static Theme,
    voice: Option<VoiceHandle>,
    voice_users: HashMap<String, Vec<String>>, // room -> nicks in voice
    room_users: HashMap<String, Vec<String>>,  // room -> nicks present
    share_screen: Option<std::process::Child>, // local screen-capture process
    share_cam: Option<std::process::Child>,    // local cam-capture process
    screen_audio: Option<audio_routing::AudioIsolation>, // Dusk sink isolation during screen share
    voice_audio: Option<audio_routing::AudioIsolation>,  // Dusk sink isolation during voice (prevents monitor loopback echo)
    shares: HashMap<String, Vec<(crate::protocol::ShareKind, String)>>, // nick -> active streams
    inbound_views: HashMap<(String, crate::protocol::ShareKind), ViewerHandle>,
    // Per-kind loopback addr the local self-view connects to. Set when we
    // spawn a share, consumed when the server echoes ShareStarted back.
    self_share_addr: HashMap<crate::protocol::ShareKind, String>,
    stream_rect: Option<Rect>,
    msg_width: u16,
    msg_height: u16,
    settings: SettingsState,
    devices: DeviceConfig,
}

impl App {
    fn new(nick: String, theme: &'static Theme) -> Self {
        let cfg = Config::load().ok().flatten();
        let devices = DeviceConfig {
            audio_in: cfg.as_ref().and_then(|c| c.audio_input.clone()),
            audio_out: cfg.as_ref().and_then(|c| c.audio_output.clone()),
            video_device: cfg.as_ref().and_then(|c| c.video_device.clone()),
        };
        let settings = SettingsState::new(
            devices.audio_in.as_deref(),
            devices.audio_out.as_deref(),
            devices.video_device.as_deref(),
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
            mode: Mode::Compose,
            pane: Pane::Composer,
            center_tab: CenterTab::Messages,
            controls_cursor: 0,
            room_cursor: 0,
            scroll: HashMap::new(),
            theme,
            voice: None,
            voice_users: HashMap::new(),
            room_users: HashMap::new(),
            share_screen: None,
            share_cam: None,
            screen_audio: None,
            voice_audio: None,
            shares: HashMap::new(),
            inbound_views: HashMap::new(),
            self_share_addr: HashMap::new(),
            stream_rect: None,
            msg_width: 0,
            msg_height: 0,
            settings,
            devices,
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
                msg_id: 0,
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
        paint_inline_shares(&app);

        let has_views = !app.inbound_views.is_empty();
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
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)), if has_views => {}
        }
    }

    if let Some(mut child) = app.share_screen.take() {
        kill_share(&mut child);
    }
    if let Some(mut child) = app.share_cam.take() {
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
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        return true;
    }
    match app.mode {
        Mode::Compose => handle_compose_key(key, app, net_tx).await,
        Mode::Select => handle_select_key(key, app).await,
        Mode::Pane => handle_pane_key(key, app, net_tx).await,
        Mode::Settings => handle_settings_key(key, app, net_tx).await,
    }
}

// Compose mode: keystrokes go to the composer. Esc enters Select mode.
async fn handle_compose_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        // Esc: leave Compose, enter Select. Default focus = Center (per plan).
        (_, Esc) => {
            app.mode = Mode::Select;
            app.pane = Pane::Center;
        }

        // Tab: legacy alias — also moves into Select-on-Rooms for quick room access.
        (_, Tab) => {
            app.mode = Mode::Select;
            app.pane = Pane::Rooms;
            app.room_cursor = app
                .rooms
                .iter()
                .position(|r| r == &app.current_room)
                .unwrap_or(0);
        }

        // Shift+Enter / Alt+Enter: insert a newline.
        (KeyModifiers::SHIFT, Enter) | (KeyModifiers::ALT, Enter) => {
            app.input.insert(app.cursor, '\n');
            app.cursor += 1;
        }

        // Enter: send the message / run the command.
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

        (_, Backspace) => {
            if app.cursor > 0 {
                let prev = prev_char_boundary(&app.input, app.cursor);
                app.input.replace_range(prev..app.cursor, "");
                app.cursor = prev;
            }
        }

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

        (KeyModifiers::CONTROL, Char('u')) => {
            app.input.clear();
            app.cursor = 0;
        }

        // V: toggle voice (only when there is nothing to send).
        (KeyModifiers::NONE, Char('v')) if app.input.is_empty() => {
            toggle_voice(app, net_tx).await;
        }

        (_, Char(c)) => {
            app.input.insert(app.cursor, c);
            app.cursor += c.len_utf8();
        }

        (_, Left) => app.cursor = prev_char_boundary(&app.input, app.cursor),
        (_, Right) => app.cursor = next_char_boundary(&app.input, app.cursor),
        (_, Up) => app.cursor = move_cursor_vertical(&app.input, app.cursor, -1),
        (_, Down) => app.cursor = move_cursor_vertical(&app.input, app.cursor, 1),
        (_, Home) => app.cursor = 0,
        (_, End) => app.cursor = app.input.len(),

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

// Select mode: arrows move focus ring through panes; Enter enters Pane mode
// (or returns to Compose if ring is on Composer); Esc returns to Compose.
async fn handle_select_key(key: KeyEvent, app: &mut App) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (_, Esc) => {
            app.mode = Mode::Compose;
            app.pane = Pane::Composer;
        }
        (_, Left) => app.pane = app.pane.move_h(-1),
        (_, Right) => app.pane = app.pane.move_h(1),
        (_, Up) => app.pane = app.pane.move_v(-1),
        (_, Down) => app.pane = app.pane.move_v(1),
        (_, Tab) => {
            // Tab cycles visually: down the left column, then over to the
            // right column, then to the composer.
            app.pane = match app.pane {
                Pane::Rooms => Pane::Controls,
                Pane::Controls => Pane::Center,
                Pane::Center => Pane::Composer,
                Pane::Composer => Pane::Rooms,
            };
        }
        (_, Enter) => {
            if app.pane == Pane::Composer {
                // Composer doesn't have a separate Pane mode — Compose IS it.
                app.mode = Mode::Compose;
            } else {
                if app.pane == Pane::Rooms {
                    app.room_cursor = app
                        .rooms
                        .iter()
                        .position(|r| r == &app.current_room)
                        .unwrap_or(0);
                }
                app.mode = Mode::Pane;
            }
        }
        _ => {}
    }
    false
}

// Pane mode: dispatch by focused pane.
async fn handle_pane_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    // Esc always pops back to Select.
    if matches!(key.code, Esc) {
        app.mode = Mode::Select;
        return false;
    }
    match app.pane {
        Pane::Rooms => handle_rooms_pane_key(key, app, net_tx).await,
        Pane::Center => handle_center_pane_key(key, app).await,
        Pane::Controls => handle_controls_pane_key(key, app, net_tx).await,
        Pane::Composer => {
            // Shouldn't normally land here — Select-on-Composer + Enter routes
            // to Compose mode. Snap back if it does.
            app.mode = Mode::Compose;
            false
        }
    }
}

async fn handle_rooms_pane_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (_, Up) => {
            if !app.rooms.is_empty() {
                app.room_cursor = if app.room_cursor == 0 {
                    app.rooms.len() - 1
                } else {
                    app.room_cursor - 1
                };
            }
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
            app.mode = Mode::Compose;
            app.pane = Pane::Composer;
        }
        _ => {}
    }
    false
}

async fn handle_center_pane_key(key: KeyEvent, app: &mut App) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (_, Left) => app.center_tab = CenterTab::Messages,
        (_, Right) => app.center_tab = CenterTab::Stream,
        (_, Up) => app.scroll_up(1),
        (_, Down) => app.scroll_down(1),
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

async fn handle_controls_pane_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (_, Up) => {
            app.controls_cursor = if app.controls_cursor == 0 {
                CONTROL_BUTTONS.len() - 1
            } else {
                app.controls_cursor - 1
            };
        }
        (_, Down) => {
            app.controls_cursor = (app.controls_cursor + 1) % CONTROL_BUTTONS.len();
        }
        (_, Enter) => {
            press_control(app, net_tx, CONTROL_BUTTONS[app.controls_cursor]).await;
        }
        _ => {}
    }
    false
}

async fn press_control(app: &mut App, net_tx: &mpsc::Sender<ClientMsg>, btn: ControlButton) {
    use crate::protocol::ShareKind;
    match btn {
        ControlButton::Voice => toggle_voice(app, net_tx).await,
        ControlButton::ShareScreen => {
            if app.share_screen.is_some() {
                share_stop(app, net_tx, Some(ShareKind::Screen)).await;
            } else {
                share_cmd(app, net_tx, Some(ShareKind::Screen)).await;
            }
        }
        ControlButton::ShareCam => {
            if app.share_cam.is_some() {
                share_stop(app, net_tx, Some(ShareKind::Cam)).await;
            } else {
                share_cmd(app, net_tx, Some(ShareKind::Cam)).await;
            }
        }
        ControlButton::Settings => {
            app.settings = SettingsState::new(
                app.devices.audio_in.as_deref(),
                app.devices.audio_out.as_deref(),
                app.devices.video_device.as_deref(),
                app.theme.name,
            );
            app.mode = Mode::Settings;
        }
    }
}

async fn handle_settings_key(key: KeyEvent, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) -> bool {
    use KeyCode::*;
    match (key.modifiers, key.code) {
        (_, Esc) | (_, Tab) => {
            apply_settings(app).await;
            app.mode = Mode::Compose;
            app.pane = Pane::Composer;
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

    app.devices.audio_in = if audio_in == "(none)" { None } else { Some(audio_in.clone()) };
    app.devices.audio_out = if audio_out == "(none)" { None } else { Some(audio_out.clone()) };
    app.devices.video_device = if video == "(none)" { None } else { Some(video.clone()) };
    app.theme = by_name(&theme_name);

    let _ = Config::update_devices(
        app.devices.audio_in.as_deref(),
        app.devices.audio_out.as_deref(),
        app.devices.video_device.as_deref(),
        &theme_name,
    );
}

async fn toggle_voice(app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) {
    if app.voice.is_some() {
        app.voice = None;
        app.voice_audio = None; // restores PipeWire routing via Drop
        let _ = net_tx.send(ClientMsg::VoiceLeave).await;
        let room = app.current_room.clone();
        app.push_sys(room, "left voice".into());
    } else {
        match crate::voice::start(net_tx.clone(), app.devices.audio_in.as_deref(), app.devices.audio_out.as_deref()) {
            Ok(handle) => {
                app.voice = Some(handle);
                // Isolate AFTER cpal opens its output stream so that pactl can
                // find and move the newly registered Dusk sink input to the null
                // sink. Running setup() before start() means no sink inputs exist
                // yet and isolation silently does nothing.
                app.voice_audio = Some(audio_routing::AudioIsolation::setup());
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

use self::commands::Command;
use self::text::{cursor_rowcol, message_lines, move_cursor_vertical, next_char_boundary, prev_char_boundary};

async fn handle_cmd(cmd: &str, app: &mut App, net_tx: &mpsc::Sender<ClientMsg>) {
    match Command::parse(cmd) {
        Command::Join(room) => {
            do_switch(app, net_tx, room).await;
        }
        Command::Create(name) => {
            let _ = net_tx.send(ClientMsg::CreateRoom { name }).await;
        }
        Command::Rooms => {
            let _ = net_tx.send(ClientMsg::ListRooms).await;
        }
        Command::Theme(Some(name)) => {
            app.theme = by_name(&name);
            let _ = Config::update_theme(&name);
            let room = app.current_room.clone();
            app.push_sys(room, format!("theme → {}", app.theme.name));
        }
        Command::Theme(None) => {
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
        Command::Share(kind) => {
            share_cmd(app, net_tx, Some(kind)).await;
        }
        Command::ShareStop => {
            share_stop(app, net_tx, None).await;
        }
        Command::Watch(arg) => {
            watch_cmd(app, arg.as_deref());
        }
        Command::Help => {
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
        Command::Unknown(verb) => {
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

// Query wf-recorder for the first available Wayland output and return
// `--output <name>` ready for shell interpolation. Returns an empty string if
// wf-recorder is unavailable or reports no outputs (env override still works).
fn detect_wl_output() -> String {
    let Ok(out) = std::process::Command::new("wf-recorder").arg("-L").output() else {
        return String::new();
    };
    // Each line: "N. Name: <output> Description: ..."
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|line| {
            let mut words = line.split_whitespace();
            while let Some(w) = words.next() {
                if w == "Name:" {
                    return words.next();
                }
            }
            None
        })
        .map(|name| format!("--output {name}"))
        .unwrap_or_default()
}

// Spawn a shell command detached from the TUI: no shared stdio, and in its own
// process group so the whole pipeline can be signalled as a unit. stderr lands
// in /tmp/dusk-share.log so a silent pipeline failure (wf-recorder missing
// $WAYLAND_DISPLAY, ffmpeg can't bind port, etc.) is observable via `tail -f`.
fn spawn_detached(cmd: &str) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/dusk-share.log")
        .ok()
        .map(Stdio::from)
        .unwrap_or_else(Stdio::null);
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
}

// Terminate the capture process and its whole group (a negative pid to `kill`
// signals the process group, catching piped children like ffmpeg).
fn kill_share(child: &mut std::process::Child) {
    let pid = child.id();
    // `--` prevents the negative PGID from being parsed as a signal flag.
    let _ = std::process::Command::new("kill")
        .args(["--", &format!("-{pid}")])
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

async fn share_cmd(
    app: &mut App,
    net_tx: &mpsc::Sender<ClientMsg>,
    kind: Option<crate::protocol::ShareKind>,
) {
    use crate::protocol::ShareKind;
    let room = app.current_room.clone();
    let share_kind = kind.unwrap_or(ShareKind::Screen);

    let already = match share_kind {
        ShareKind::Screen => app.share_screen.is_some(),
        ShareKind::Cam => app.share_cam.is_some(),
    };
    if already {
        let label = match share_kind { ShareKind::Screen => "screen", ShareKind::Cam => "cam" };
        app.push_sys(room, format!("already sharing {label}"));
        return;
    }

    let Some(addr) = share_addr() else {
        app.push_sys(room, "share: could not detect Tailscale IP".into());
        return;
    };
    // Port plan (one listener per output):
    //   SHARE_PORT     — screen peer (public)
    //   SHARE_PORT + 1 — cam peer    (public)
    //   SHARE_PORT + 2 — screen self (loopback)
    //   SHARE_PORT + 3 — cam self    (loopback)
    let (addr, self_addr) = match share_kind {
        ShareKind::Screen => (
            addr.clone(),
            format!("127.0.0.1:{}", SHARE_PORT + 2),
        ),
        ShareKind::Cam => (
            addr.rsplit_once(':').map(|(h, _)| format!("{h}:{}", SHARE_PORT + 1)).unwrap_or(addr.clone()),
            format!("127.0.0.1:{}", SHARE_PORT + 3),
        ),
    };

    let kind_str = match share_kind {
        ShareKind::Cam => "cam",
        ShareKind::Screen => "screen",
    };
    let audio_src = app.devices.audio_in.as_deref().unwrap_or("default");
    let output_flag = if share_kind == ShareKind::Screen { detect_wl_output() } else { String::new() };
    let cmd = share_template(kind_str, app.devices.video_device.as_deref())
        .replace("{addr}", &addr)
        .replace("{self_addr}", &self_addr)
        .replace("{audio_src}", audio_src)
        .replace("{output_flag}", &output_flag);
    crate::debug_log::log(format!(
        "share[{kind_str}] spawn addr={addr} self_addr={self_addr} cmd=`{cmd}`"
    ));
    // Isolate Dusk's own audio output from the monitor before capture starts
    // so viewers don't hear call audio echoed back through the screen share.
    if share_kind == ShareKind::Screen {
        app.screen_audio = Some(audio_routing::AudioIsolation::setup());
    }

    match spawn_detached(&cmd) {
        Ok(child) => {
            let pid = child.id();
            crate::debug_log::log(format!("share[{kind_str}] pid={pid} spawned"));
            match share_kind {
                ShareKind::Screen => app.share_screen = Some(child),
                ShareKind::Cam => app.share_cam = Some(child),
            }
            // Stash the loopback addr so ShareStarted can launch our self-view.
            app.self_share_addr.insert(share_kind, self_addr.clone());
            let _ = net_tx.send(ClientMsg::ShareStart { kind: share_kind, url: addr.clone() }).await;
            app.push_sys(room, format!("sharing {kind_str} at {addr}"));
        }
        Err(e) => {
            crate::debug_log::log(format!("share[{kind_str}] spawn err: {e}"));
            app.screen_audio = None; // undo isolation if spawn failed
            app.push_sys(room, format!("share error: {e}"));
        }
    }
}

async fn share_stop(
    app: &mut App,
    net_tx: &mpsc::Sender<ClientMsg>,
    kind: Option<crate::protocol::ShareKind>,
) {
    use crate::protocol::ShareKind;
    let room = app.current_room.clone();
    let kinds: Vec<ShareKind> = match kind {
        Some(k) => vec![k],
        None => vec![ShareKind::Screen, ShareKind::Cam],
    };
    let mut stopped: Vec<&'static str> = Vec::new();
    for k in &kinds {
        let slot = match k { ShareKind::Screen => &mut app.share_screen, ShareKind::Cam => &mut app.share_cam };
        if let Some(mut child) = slot.take() {
            kill_share(&mut child);
            app.self_share_addr.remove(k);
            if *k == ShareKind::Screen {
                app.screen_audio = None; // Drop restores PipeWire routing
            }
            stopped.push(match k { ShareKind::Screen => "screen", ShareKind::Cam => "cam" });
        }
    }
    if stopped.is_empty() {
        app.push_sys(room, "not currently sharing".into());
        return;
    }
    // Send the precise kind(s) actually stopped so peers display the right label.
    let stopped_kind = if kinds.len() == 1 { Some(kinds[0]) } else { kind };
    let _ = net_tx.send(ClientMsg::ShareStop { kind: stopped_kind }).await;
    app.push_sys(room, format!("stopped sharing {}", stopped.join(", ")));
}

fn watch_cmd(app: &mut App, arg: Option<&str>) {
    let room = app.current_room.clone();

    // Pick first stream from the named (or only) sharer.
    let url: Option<String> = match arg {
        Some(nick) => app.shares.get(nick).and_then(|v| v.first().map(|(_, u)| u.clone())),
        None if app.shares.len() == 1 => app
            .shares
            .values()
            .next()
            .and_then(|v| v.first().map(|(_, u)| u.clone())),
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

        ServerMsg::Message { room, msg_id, nick, content, ts } => {
            app.push_msg(&room, ChatMessage { msg_id, nick, content, ts });
        }

        ServerMsg::UserJoined { room, nick, users } => {
            app.room_users.insert(room.clone(), users);
            app.push_sys(room, format!("→ {nick} joined"));
        }

        ServerMsg::UserLeft { room, nick, users } => {
            app.room_users.insert(room.clone(), users);
            app.shares.remove(&nick);
            app.inbound_views.retain(|(n, _), _| n != &nick);
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

        ServerMsg::VoiceFrame { nick, data } => {
            if let Some(ref v) = app.voice {
                if let Ok(bytes) = STANDARD.decode(&data) {
                    let _ = v.frame_in.try_send((nick, bytes));
                }
            }
        }

        ServerMsg::ShareStarted { room, nick, kind, url } => {
            // Record the share for everyone (sharer included) so the Stream tab
            // reflects reality. The sharer connects to a loopback that ffmpeg's
            // `tee` muxer fans out alongside the public peer endpoint, so we
            // never race the peer for a single `-listen 1` socket.
            let is_self = nick == app.nick;
            let entry = app.shares.entry(nick.clone()).or_default();
            entry.retain(|(k, _)| *k != kind);
            entry.push((kind, url.clone()));
            let label = match kind {
                crate::protocol::ShareKind::Screen => "screen",
                crate::protocol::ShareKind::Cam => "cam",
            };
            if room == app.current_room {
                app.center_tab = CenterTab::Stream;
            }
            crate::debug_log::log(format!(
                "ShareStarted nick={nick} kind={label} url={url} is_self={is_self} kitty={}",
                is_kitty()
            ));
            if is_kitty() {
                let view_url = if is_self {
                    app.self_share_addr.get(&kind).cloned()
                } else {
                    Some(url.clone())
                };
                if let Some(view_url) = view_url {
                    crate::debug_log::log(format!(
                        "viewer launch for {nick} ({label}) -> {view_url}"
                    ));
                    if let Some(v) = ViewerHandle::spawn(view_url, kind) {
                        app.inbound_views.insert((nick.clone(), kind), v);
                    }
                } else if is_self {
                    crate::debug_log::log(format!(
                        "self ShareStarted but no self_share_addr stashed for {label}"
                    ));
                }
            }
            if !is_self {
                app.push_sys(room, format!("{nick} is sharing {label} — Stream tab"));
            }
        }

        ServerMsg::ShareStopped { room, nick, kind } => {
            let is_self = nick == app.nick;
            crate::debug_log::log(format!(
                "ShareStopped nick={nick} kind={kind:?} is_self={is_self}"
            ));
            match kind {
                Some(k) => {
                    if let Some(v) = app.shares.get_mut(&nick) {
                        v.retain(|(kk, _)| *kk != k);
                        if v.is_empty() { app.shares.remove(&nick); }
                    }
                    app.inbound_views.remove(&(nick.clone(), k));
                    if is_self { app.self_share_addr.remove(&k); }
                }
                None => {
                    app.shares.remove(&nick);
                    app.inbound_views.retain(|(n, _), _| n != &nick);
                    if is_self { app.self_share_addr.clear(); }
                }
            }
            if !is_self {
                let label = match kind {
                    Some(crate::protocol::ShareKind::Screen) => "screen",
                    Some(crate::protocol::ShareKind::Cam) => "cam",
                    None => "stream",
                };
                app.push_sys(room, format!("{nick} stopped {label}"));
            }
        }

        ServerMsg::Error { msg } => {
            app.status = Some(format!("error: {msg}"));
        }

        ServerMsg::Pong { .. } => {}
        ServerMsg::Hello { .. } => {}
        ServerMsg::History { .. } => {}
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

    let [sidebar, center_area] =
        Layout::horizontal([Constraint::Length(22), Constraint::Min(1)]).areas(body);

    // Sidebar stacks Rooms (flexible, collapses first) over a fixed-height
    // Controls block. Controls: 4 buttons + top/bottom borders = 6 rows.
    let [rooms_area, controls_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).areas(sidebar);

    draw_active_bar(f, app, active_area);
    draw_rooms(f, app, rooms_area);
    draw_center(f, app, center_area);
    draw_composer(f, app, input_area);
    draw_controls(f, app, controls_area);
    draw_hints(f, app, hints_area);

    if app.mode == Mode::Settings {
        draw_settings(f, app, area);
    }
}

fn draw_rooms(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let focused = app.mode != Mode::Compose && app.pane == Pane::Rooms;
    let active = app.mode == Mode::Pane && app.pane == Pane::Rooms;

    let items: Vec<ListItem> = app
        .rooms
        .iter()
        .enumerate()
        .map(|(i, room)| {
            let is_current = room == &app.current_room;
            let is_cursor = active && i == app.room_cursor;
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

    f.render_widget(List::new(items).block(block), area);
}

fn draw_center(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let focused = app.mode != Mode::Compose && app.pane == Pane::Center;

    let border_style = if focused {
        Style::default().fg(t.border_focus)
    } else {
        Style::default().fg(t.border_dim)
    };

    // Has anyone in the room got an active share? Used to badge the Stream tab.
    let any_share = app
        .room_users
        .get(&app.current_room)
        .map(|users| users.iter().any(|u| app.shares.contains_key(u)))
        .unwrap_or(false);

    let tab = |label: &str, active: bool| {
        let style = if active {
            Style::default().fg(t.border_focus).add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(t.room_inactive)
        };
        Span::styled(format!(" {label} "), style)
    };

    let messages_tab = tab("Messages", app.center_tab == CenterTab::Messages);
    let stream_label = if any_share { "Stream ●" } else { "Stream" };
    let stream_tab = tab(stream_label, app.center_tab == CenterTab::Stream);

    let scroll_label = {
        let raw = app.current_scroll();
        if raw > 0 { format!(" ↑{raw}") } else { String::new() }
    };
    let voice_label = app
        .voice_users
        .get(&app.current_room)
        .filter(|u| !u.is_empty())
        .map(|u| format!(" [mic {}] ", u.join(" ")))
        .unwrap_or_default();
    let title_room = Span::styled(
        format!(" #{}{}{} ", app.current_room, voice_label, scroll_label),
        Style::default().fg(t.border_focus).add_modifier(Modifier::BOLD),
    );

    let title_line = Line::from(vec![title_room, Span::raw("│"), messages_tab, stream_tab]);

    let block = Block::default()
        .title(title_line)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    match app.center_tab {
        CenterTab::Messages => {
            app.stream_rect = None;
            draw_messages_body(f, app, inner);
        }
        CenterTab::Stream => draw_stream_body(f, app, inner),
    }
}

fn draw_messages_body(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let inner_w = area.width as usize;
    let inner_h = area.height as usize;
    app.msg_width = inner_w as u16;
    app.msg_height = inner_h as u16;

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

    f.render_widget(Paragraph::new(Text::from(visible)), area);
}

fn draw_stream_body(f: &mut Frame, app: &mut App, area: Rect) {
    app.stream_rect = Some(area);
    let t = app.theme;
    let sharers: Vec<(String, Vec<crate::protocol::ShareKind>)> = app
        .room_users
        .get(&app.current_room)
        .map(|users| {
            users
                .iter()
                .filter_map(|u| {
                    app.shares
                        .get(u)
                        .map(|v| (u.clone(), v.iter().map(|(k, _)| *k).collect()))
                })
                .collect()
        })
        .unwrap_or_default();

    let lines: Vec<Line> = if sharers.is_empty() {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "no active streams",
                Style::default().fg(t.hint_text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "press the Share Screen or Share Cam button to start one",
                Style::default().fg(t.room_inactive),
            )),
        ]
    } else {
        let mut out = vec![Line::from(Span::styled(
            "active streams:",
            Style::default().fg(t.hint_text).add_modifier(Modifier::BOLD),
        ))];
        for (s, kinds) in &sharers {
            let kind_str: Vec<&str> = kinds
                .iter()
                .map(|k| match k {
                    crate::protocol::ShareKind::Screen => "screen",
                    crate::protocol::ShareKind::Cam => "cam",
                })
                .collect();
            // Surface per-kind framing state: live (frames arriving), connecting
                // (handle spawned, no frame yet), or off (no viewer for this kind —
            // self-share, or non-kitty terminal).
            let state: Vec<&str> = kinds
                .iter()
                .map(|k| {
                    match app.inbound_views.get(&(s.clone(), *k)) {
                        Some(h) => match h.frame.lock().ok().map(|g| g.is_some()) {
                            Some(true) => "live",
                            _ => "connecting",
                        },
                        None => "off",
                    }
                })
                .collect();
            let label = if s == &app.nick {
                format!("  ● {s} (you) — {} [{}]", kind_str.join(" + "), state.join(" + "))
            } else {
                format!("  ● {s} — {} [{}]", kind_str.join(" + "), state.join(" + "))
            };
            out.push(Line::from(Span::styled(label, Style::default().fg(t.msg_nick))));
        }
        out.push(Line::from(""));
        let hint = if !is_kitty() {
            "/watch <nick> to open external viewer  (audio included)"
        } else {
            "frames render here when [live]  ·  /watch <nick> for audio"
        };
        out.push(Line::from(Span::styled(
            hint,
            Style::default().fg(t.room_inactive),
        )));
        out
    };

    f.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        area,
    );
}

fn is_kitty() -> bool {
    std::env::var("TERM").map(|t| t == "xterm-kitty").unwrap_or(false)
        || std::env::var("KITTY_WINDOW_ID").is_ok()
}

// After ratatui commits its frame, overlay decoded video frames onto the Stream
// pane via the Kitty graphics protocol. Streams are arranged in a grid:
//   1 → full pane   2 → side by side   4 → 2×2   6 → 3×2   9 → 3×3 …
fn paint_inline_shares(app: &App) {
    if !matches!(app.center_tab, CenterTab::Stream) { return; }
    if !is_kitty() { return; }
    let Some(rect) = app.stream_rect else { return };
    if app.inbound_views.is_empty() || rect.height < 4 || rect.width < 4 { return; }

    // Collect only views that already have a decoded frame.
    let ready: Vec<image::DynamicImage> = app
        .inbound_views
        .values()
        .filter_map(|h| h.frame.lock().ok().and_then(|g| g.clone()))
        .collect();
    if ready.is_empty() { return; }

    let (cols, rows) = grid_dims(ready.len());
    let cell_w = rect.width / cols as u16;
    let cell_h = rect.height / rows as u16;
    if cell_w < 2 || cell_h < 2 { return; }

    for (i, img) in ready.iter().enumerate() {
        let col = (i % cols) as u16;
        let row = (i / cols) as u16;
        let cfg = viuer::Config {
            x: rect.x + col * cell_w,
            y: (rect.y + row * cell_h) as i16,
            width: Some(cell_w as u32),
            height: Some(cell_h as u32),
            absolute_offset: true,
            use_kitty: true,
            use_iterm: false,
            restore_cursor: true,
            ..Default::default()
        };
        let _ = viuer::print(img, &cfg);
    }
}

// Compute grid dimensions (cols, rows) for n streams.
// Targets a roughly square layout: 1→1×1, 2→2×1, 3-4→2×2, 5-6→3×2, 7-9→3×3 …
fn grid_dims(n: usize) -> (usize, usize) {
    if n <= 1 { return (1, 1); }
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = (n + cols - 1) / cols;
    (cols, rows)
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

fn draw_composer(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    // Composer is "focused" any time we're typing into it (Compose mode) or the
    // focus ring is on it in Select mode.
    let focused = app.mode == Mode::Compose
        || (app.mode != Mode::Settings && app.pane == Pane::Composer);
    let cursor_visible = app.mode == Mode::Compose;

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

    if cursor_visible && inner_h > 0 {
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

fn draw_controls(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let focused = app.mode != Mode::Compose && app.pane == Pane::Controls;
    let active = app.mode == Mode::Pane && app.pane == Pane::Controls;

    let voice_on = app.voice.is_some();
    let screen_on = app.share_screen.is_some();
    let cam_on = app.share_cam.is_some();

    let lines: Vec<Line> = CONTROL_BUTTONS
        .iter()
        .enumerate()
        .map(|(i, btn)| {
            let (label, state) = match btn {
                ControlButton::Voice => ("Voice       ", Some(voice_on)),
                ControlButton::ShareScreen => ("Share Screen", Some(screen_on)),
                ControlButton::ShareCam => ("Share Cam   ", Some(cam_on)),
                ControlButton::Settings => ("Settings    ", None),
            };
            let is_cursor = active && i == app.controls_cursor;
            let arrow = if is_cursor { "▶ " } else { "  " };
            let state_str = match state {
                Some(true) => "ON ",
                Some(false) => "OFF",
                None => "   ",
            };
            let label_style = if is_cursor {
                Style::default().fg(t.room_active).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.msg_text)
            };
            let state_style = match state {
                Some(true) => Style::default().fg(t.room_active).add_modifier(Modifier::BOLD),
                Some(false) => Style::default().fg(t.room_inactive),
                None => Style::default().fg(t.hint_text),
            };
            Line::from(vec![
                Span::styled(arrow, Style::default().fg(t.border_focus)),
                Span::styled(label, label_style),
                Span::raw(" "),
                Span::styled(state_str, state_style),
            ])
        })
        .collect();

    let border_style = if focused {
        Style::default().fg(t.border_focus)
    } else {
        Style::default().fg(t.border_dim)
    };

    let block = Block::default()
        .title(Span::styled(
            " controls ",
            Style::default().fg(t.border_focus),
        ))
        .borders(Borders::ALL)
        .border_style(border_style);

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
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

    let pane_label = |p: Pane| match p {
        Pane::Rooms => "rooms",
        Pane::Center => match app.center_tab {
            CenterTab::Messages => "messages",
            CenterTab::Stream => "stream",
        },
        Pane::Composer => "composer",
        Pane::Controls => "controls",
    };

    let (mode_tag, hints) = match app.mode {
        Mode::Compose => (
            "[compose]".to_string(),
            format!(" esc:select  shift+↵:newline  pgup/dn:scroll  /help{voice_hint}  ^c:quit"),
        ),
        Mode::Select => (
            format!("[select: {}]", pane_label(app.pane)),
            " ←→↑↓:move  enter:focus  esc:compose  ^c:quit".to_string(),
        ),
        Mode::Pane => {
            let body = match app.pane {
                Pane::Rooms => " ↑↓:browse  enter:join  esc:back",
                Pane::Center => " ←→:switch tab  ↑↓:scroll  pgup/dn:page  esc:back",
                Pane::Controls => " ↑↓:select  enter:toggle  esc:back",
                Pane::Composer => " esc:back",
            };
            (format!("[pane: {}]", pane_label(app.pane)), body.to_string())
        }
        Mode::Settings => (
            "[settings]".to_string(),
            " ↑↓:navigate  ←→:change  tab/esc:save & close".to_string(),
        ),
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" {mode_tag} "),
            Style::default().fg(t.border_focus).add_modifier(Modifier::BOLD),
        ),
        Span::styled(hints, Style::default().fg(t.hint_text)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
