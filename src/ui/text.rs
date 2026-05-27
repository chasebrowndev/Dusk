//! Pure text helpers: cursor math and word-wrapping for the message pane.

use chrono::{Local, TimeZone};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::protocol::ChatMessage;
use crate::theme::Theme;

// Byte offset of the char boundary just before `idx`.
pub(crate) fn prev_char_boundary(s: &str, idx: usize) -> usize {
    s[..idx].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
}

// Byte offset of the char boundary just after `idx`.
pub(crate) fn next_char_boundary(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map(|c| idx + c.len_utf8()).unwrap_or(idx)
}

// Row (0-based line) and column (0-based char) of `cursor` within `input`.
pub(crate) fn cursor_rowcol(input: &str, cursor: usize) -> (usize, usize) {
    let before = &input[..cursor];
    let row = before.bytes().filter(|&b| b == b'\n').count();
    let col = before.rsplit('\n').next().unwrap_or("").chars().count();
    (row, col)
}

// Move the cursor up (dir = -1) or down (dir = 1) one line, keeping the column.
pub(crate) fn move_cursor_vertical(input: &str, cursor: usize, dir: i32) -> usize {
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

// Wrap a message into styled display lines that each fit within `width`
// columns. System messages (empty nick) are indented; chat messages keep a
// `HH:MM nick:` header on the first line and indent continuation lines.
// Explicit newlines in the content start a fresh wrapped segment.
pub(crate) fn message_lines(m: &ChatMessage, width: usize, t: &Theme) -> Vec<Line<'static>> {
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
pub(crate) fn wrap_words(text: &str, first: usize, rest: usize) -> Vec<String> {
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
