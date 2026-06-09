use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};

use crate::config::{Config, Role};

const SPLASH_ART: &[&str] = &[
    "·▄▄▄▄  ▄• ▄▌.▄▄ · ▄ •▄ ",
    "██▪ ██ █▪██▌▐█ ▀. █▌▄▌▪",
    "▐█· ▐█▌█▌▐█▌▄▀▀▀█▄▐▀▀▄·",
    "██. ██ ▐█▄█▌▐█▄▪▐█▐█.█▌",
    "▀▀▀▀▀•  ▀▀▀  ▀▀▀▀ ·▀  ▀",
];

#[derive(PartialEq)]
enum Step {
    Nick,
    Mode,
    Server,
    Done,
}

struct Wizard {
    step: Step,
    nick: String,
    role: Role,
    server: String,
    error: Option<&'static str>,
}

impl Wizard {
    fn new() -> Self {
        Wizard {
            step: Step::Nick,
            nick: String::new(),
            role: Role::Client,
            server: String::new(),
            error: None,
        }
    }

    fn step_label(&self) -> &'static str {
        match self.step {
            Step::Nick => "1/3",
            Step::Mode => "2/3",
            Step::Server => "3/3",
            Step::Done => "done",
        }
    }

    fn advance(&mut self) {
        self.error = None;
        match self.step {
            Step::Nick => {
                let clean: String = self
                    .nick
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                    .take(20)
                    .collect();
                if clean.is_empty() {
                    self.error = Some("nickname cannot be empty");
                    return;
                }
                self.nick = clean;
                self.step = Step::Mode;
            }
            Step::Mode => {
                self.step = if self.role == Role::Client {
                    Step::Server
                } else {
                    Step::Done
                };
            }
            Step::Server => {
                self.step = Step::Done;
            }
            Step::Done => {}
        }
    }

    fn build_config(&self) -> Config {
        let server = self.server.trim();
        let server = if server.is_empty() {
            None
        } else if server.contains(':') {
            Some(server.to_string())
        } else {
            Some(format!("{server}:7667"))
        };
        Config {
            nick: self.nick.clone(),
            machine_id: Config::machine_id(),
            theme: "cyberpunk".into(),
            server,
            role: self.role.clone(),
            audio_input: None,
            audio_output: None,
            video_device: None,
        }
    }
}

pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut events = EventStream::new();
    let mut wiz = Wizard::new();

    loop {
        terminal.draw(|f| draw(f, &wiz))?;

        if wiz.step == Step::Done {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            break;
        }

        let Some(Ok(Event::Key(key))) = events.next().await else {
            continue;
        };

        use KeyCode::*;
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, Char('c')) => {
                cleanup(&mut terminal)?;
                return Err(anyhow::anyhow!("setup cancelled"));
            }

            (_, Backspace) => match wiz.step {
                Step::Nick => {
                    wiz.nick.pop();
                }
                Step::Server => {
                    wiz.server.pop();
                }
                _ => {}
            },

            (KeyModifiers::CONTROL, Char('u')) => match wiz.step {
                Step::Nick => wiz.nick.clear(),
                Step::Server => wiz.server.clear(),
                _ => {}
            },

            (KeyModifiers::CONTROL, Char('w')) => match wiz.step {
                Step::Nick => trim_last_word(&mut wiz.nick),
                Step::Server => trim_last_word(&mut wiz.server),
                _ => {}
            },

            (_, Left) | (_, Char('h')) if wiz.step == Step::Mode => {
                wiz.role = Role::Client;
                wiz.error = None;
            }
            (_, Right) | (_, Char('l')) if wiz.step == Step::Mode => {
                wiz.role = Role::Host;
                wiz.error = None;
            }

            (_, Enter) => {
                wiz.advance();
                if wiz.step == Step::Done {
                    let cfg = wiz.build_config();
                    cfg.save()?;
                    terminal.draw(|f| draw(f, &wiz))?;
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    break;
                }
            }

            (_, Char(c)) => match wiz.step {
                Step::Nick => wiz.nick.push(c),
                Step::Server => wiz.server.push(c),
                _ => {}
            },

            _ => {}
        }
    }

    cleanup(&mut terminal)
}

fn trim_last_word(s: &mut String) {
    let trimmed = s.trim_end_matches([' ', '\t']).len();
    let cut = s[..trimmed].rfind([' ', '\t']).map(|i| i + 1).unwrap_or(0);
    s.truncate(cut);
}

fn cleanup(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw(f: &mut Frame, wiz: &Wizard) {
    let area = f.area();

    let box_w = 54u16.min(area.width);
    let box_h = 16u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(box_w)) / 2;
    let y = area.y + (area.height.saturating_sub(box_h)) / 2;
    let rect = Rect { x, y, width: box_w, height: box_h };

    f.render_widget(Clear, rect);

    let step_label = wiz.step_label();
    let title = format!(" dusk — setup [{step_label}] ");
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(block, rect);

    let inner = Rect {
        x: rect.x + 2,
        y: rect.y + 1,
        width: rect.width.saturating_sub(4),
        height: rect.height.saturating_sub(2),
    };

    let mut lines: Vec<Line> = SPLASH_ART
        .iter()
        .map(|&l| Line::from(Span::styled(l, Style::default().fg(Color::Cyan))))
        .collect();
    lines.push(Line::from(""));

    match wiz.step {
        Step::Nick => {
            lines.push(Line::from(Span::styled(
                "choose your nickname",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{}_", wiz.nick),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(""));
            if let Some(err) = wiz.error {
                lines.push(Line::from(Span::styled(
                    err,
                    Style::default().fg(Color::Red),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "enter to continue   ^c to quit",
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }

        Step::Mode => {
            lines.push(Line::from(Span::styled(
                "how will you use dusk?",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));

            let (client_style, host_style) = match wiz.role {
                Role::Client => (
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                    Style::default().fg(Color::DarkGray),
                ),
                Role::Host => (
                    Style::default().fg(Color::DarkGray),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                ),
            };
            lines.push(Line::from(vec![
                Span::styled("  [ CLIENT ]  ", client_style),
                Span::raw("    "),
                Span::styled("  [ HOST ]  ", host_style),
            ]));
            lines.push(Line::from(""));

            let role_hint = match wiz.role {
                Role::Client => "connect to a hub server",
                Role::Host => "run the hub and connect locally",
            };
            lines.push(Line::from(Span::styled(
                role_hint,
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "←→ to switch   enter to confirm",
                Style::default().fg(Color::DarkGray),
            )));
        }

        Step::Server => {
            lines.push(Line::from(Span::styled(
                "tailscale machine name or address",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{}_", wiz.server),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "e.g. chase-pc  or  chase-pc:7667  or blank to scan",
                Style::default().fg(Color::DarkGray),
            )));
        }

        Step::Done => {
            lines.push(Line::from(Span::styled(
                "config saved.",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "launching dusk...",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        inner,
    );
}
