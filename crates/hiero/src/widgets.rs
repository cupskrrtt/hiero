//! Small pieces the workspace screen shares: a one-line text input, popups,
//! and painting a terminal screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::num::NonZeroU16;

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    /// Cursor position in chars.
    cursor: usize,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        TextInput { text, cursor }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.text.len())
    }

    /// Insert pasted text at the cursor (newlines become spaces).
    pub fn insert_str(&mut self, text: &str) {
        let clean: String = text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
        let b = self.byte_at(self.cursor);
        self.text.insert_str(b, &clean);
        self.cursor += clean.chars().count();
    }

    /// Edit on a key; false when the key isn't for the input.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char('u') if ctrl => {
                let b = self.byte_at(self.cursor);
                self.text.replace_range(..b, "");
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                let (a, b) = (self.byte_at(start), self.byte_at(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char(_) if ctrl => return false,
            KeyCode::Char(c) => {
                let b = self.byte_at(self.cursor);
                self.text.insert(b, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let b = self.byte_at(self.cursor - 1);
                self.text.remove(b);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let b = self.byte_at(self.cursor);
                self.text.remove(b);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

/// A pane's program, drawn from its vt100 screen.
pub struct PaneView {
    pub addr: String,
    pub parser: vt100::Parser,
    /// Set when the stream ended; the pane stays until you leave it.
    pub ended: Option<String>,
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

impl PaneView {
    pub fn new(addr: String, cols: u16, rows: u16) -> Self {
        PaneView {
            addr,
            parser: vt100::Parser::new(nz(rows), nz(cols), 0),
            ended: None,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(nz(rows), nz(cols));
    }
}


/// A centered box of `w`×`h` cells, cleared.
pub fn popup(f: &mut Frame, area: Rect, w: u16, h: u16, title: &str) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(Clear, r);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(format!(" {title} "), Style::new().bold()));
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

/// One text field; puts the cursor in it when focused.
pub fn draw_input(f: &mut Frame, area: Rect, input: &TextInput, focused: bool) {
    let style = if focused { Style::new().bg(Color::Rgb(40, 44, 52)) } else { Style::new().dim() };
    let text = input.value();
    // Scroll horizontally so the cursor stays visible.
    let before: String = text.chars().take(input.cursor()).collect();
    let before_w = Span::raw(before.as_str()).width() as u16;
    let skip = before_w.saturating_sub(area.width.saturating_sub(1));
    f.render_widget(Paragraph::new(text).style(style).scroll((0, skip)), area);
    if focused {
        f.set_cursor_position((area.x + before_w - skip, area.y));
    }
}

/// Paint a terminal screen into `inner`, and put the cursor there if asked.
pub fn draw_vt(f: &mut Frame, parser: &vt100::Parser, inner: Rect, show_cursor: bool) {
    let screen = parser.screen();
    let buf = f.buffer_mut();
    let (rows, cols) = screen.size();
    for r in 0..rows.get().min(inner.height) {
        for c in 0..cols.get().min(inner.width) {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut fg = vt_color(cell.fgcolor());
            let mut bg = vt_color(cell.bgcolor());
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
                fg = fg.or(Some(Color::Black));
                bg = bg.or(Some(Color::White));
            }
            let mut style = Style::new();
            if let Some(c) = fg {
                style = style.fg(c);
            }
            if let Some(c) = bg {
                style = style.bg(c);
            }
            let mut m = Modifier::empty();
            if cell.bold() {
                m |= Modifier::BOLD;
            }
            if cell.dim() {
                m |= Modifier::DIM;
            }
            if cell.italic() {
                m |= Modifier::ITALIC;
            }
            if cell.underline() {
                m |= Modifier::UNDERLINED;
            }
            let pos = (inner.x + c, inner.y + r);
            if let Some(out) = buf.cell_mut(pos) {
                let text = cell.contents();
                out.set_symbol(if text.is_empty() { " " } else { text });
                out.set_style(style.add_modifier(m));
            }
        }
    }
    if show_cursor && !screen.hide_cursor() {
        let (r, c) = screen.cursor_position();
        if r < inner.height && c < inner.width {
            f.set_cursor_position((inner.x + c, inner.y + r));
        }
    }
}

fn vt_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

pub fn describe_close(reason: Option<&serde_json::Value>) -> String {
    let Some(r) = reason else {
        return "stream closed".into();
    };
    let why = r.get("reason").and_then(|v| v.as_str()).unwrap_or("closed");
    match r.get("exit_code").and_then(|v| v.as_i64()) {
        Some(code) => format!("{why} (code {code})"),
        None => why.replace('_', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    #[test]
    fn text_input_edits() {
        let mut t = TextInput::new("hello world");
        t.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.value(), "hello ");
        t.handle(key(KeyCode::Home));
        t.handle(key(KeyCode::Char('¡')));
        assert_eq!(t.value(), "¡hello ");
        t.handle(key(KeyCode::Backspace));
        t.handle(key(KeyCode::Delete));
        assert_eq!(t.value(), "ello ");
    }
}
