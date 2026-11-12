//! Copy mode: scroll back through a pane's history, select text, copy it.
//!
//! Places are line ids, not screen rows: line `id` is index
//! `id - (lines_scrolled - scrollback_len)` of the history-plus-screen (see
//! `vt100::Screen::lines_scrolled`). New output moves lines up the screen and
//! into the history but keeps their ids, so the view, the cursor and the
//! selection stay on the same text while the program keeps printing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::widgets::TextInput;

/// A cell of a pane: a line id and a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: u16,
}

/// The `/` or `?` prompt being typed.
pub struct Prompt {
    pub forward: bool,
    pub input: TextInput,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Stay,
    /// Leave copy mode.
    Exit,
    /// Copy this text, then leave.
    Copy(String),
    /// Stay, and say this.
    Note(String),
}

pub struct Copy {
    pub pane: String,
    /// The pane's size and stream when copy mode began: a resize or a new
    /// stream renumbers the lines, so copy mode ends then.
    pub size: (u16, u16),
    pub attempt: u64,
    /// The line at the top of the view.
    top: usize,
    pub cur: Pos,
    /// Where the selection began, while selecting.
    pub anchor: Option<Pos>,
    /// `V`: whole lines.
    pub whole_lines: bool,
    /// Begun by the wheel: scrolling back to the bottom ends it.
    pub from_wheel: bool,
    /// The last search and its direction, for `n` / `N`.
    last: Option<(String, bool)>,
    pub prompt: Option<Prompt>,
}

/// The id of index 0 of the history-plus-screen.
fn base(s: &vt100::Screen) -> usize {
    s.lines_scrolled().saturating_sub(s.scrollback_len())
}

fn rows(s: &vt100::Screen) -> usize {
    usize::from(s.size().0.get())
}

fn cols(s: &vt100::Screen) -> u16 {
    s.size().1.get()
}

/// The text of a line and the column each byte of it starts at.
fn line(s: &vt100::Screen, id: usize) -> (String, Vec<u16>) {
    let mut text = String::new();
    let mut at = Vec::new();
    let Some(i) = id.checked_sub(base(s)) else { return (text, at) };
    for col in 0..cols(s) {
        let Some(cell) = s.line_cell(i, col) else { break };
        if cell.is_wide_continuation() {
            continue;
        }
        let c = cell.contents();
        let c = if c.is_empty() { " " } else { c };
        text.push_str(c);
        at.extend(std::iter::repeat_n(col, c.len()));
    }
    // Trailing blanks are not text.
    let keep = text.trim_end().len();
    text.truncate(keep);
    at.truncate(keep);
    (text, at)
}

/// Columns `from..=to` of a line, without trailing blanks.
fn cut(s: &vt100::Screen, id: usize, from: u16, to: u16) -> String {
    let (text, at) = line(s, id);
    let start = at.iter().position(|&c| c >= from).unwrap_or(text.len());
    let end = at.iter().position(|&c| c > to).unwrap_or(text.len());
    text[start..end.max(start)].to_string()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Smart case: a query with no capitals matches either case.
fn find_in(hay: &str, needle: &str) -> Vec<usize> {
    let fold = !needle.chars().any(char::is_uppercase);
    let (h, n) = if fold { (hay.to_lowercase(), needle.to_lowercase()) } else { (hay.to_string(), needle.to_string()) };
    // Lowercasing can change byte lengths; only trust positions when it didn't.
    if h.len() != hay.len() {
        return hay.match_indices(needle).map(|(i, _)| i).collect();
    }
    h.match_indices(&n).map(|(i, _)| i).collect()
}

impl Copy {
    /// Copy mode over `s`, at the bottom, the cursor where the program's is.
    pub fn new(pane: String, size: (u16, u16), attempt: u64, s: &vt100::Screen) -> Copy {
        let bottom = s.lines_scrolled();
        let (r, c) = s.cursor_position();
        Copy {
            pane,
            size,
            attempt,
            top: bottom,
            cur: Pos { line: bottom + usize::from(r), col: c },
            anchor: None,
            whole_lines: false,
            from_wheel: false,
            last: None,
            prompt: None,
        }
    }

    /// Lines between the view and the bottom.
    pub fn lines_up(&self, s: &vt100::Screen) -> usize {
        s.lines_scrolled().saturating_sub(self.top)
    }

    /// Where the cursor is drawn: (col, row) in the view, if it is in it.
    pub fn cursor_cell(&self, s: &vt100::Screen) -> Option<(u16, u16)> {
        let r = self.cur.line.checked_sub(self.top)?;
        (r < rows(s)).then_some((self.cur.col, r as u16))
    }

    /// The selected columns of view row `r`, inclusive.
    pub fn selected(&self, s: &vt100::Screen, r: u16) -> Option<(u16, u16)> {
        let anchor = self.anchor?;
        let id = self.top + usize::from(r);
        let (a, b) = if anchor <= self.cur { (anchor, self.cur) } else { (self.cur, anchor) };
        if id < a.line || id > b.line {
            return None;
        }
        let last = cols(s) - 1;
        if self.whole_lines {
            return Some((0, last));
        }
        let from = if id == a.line { a.col } else { 0 };
        let to = if id == b.line { b.col } else { last };
        Some((from, to.min(last)))
    }

    /// Keep everything on lines that still exist, then show the view.
    pub fn apply(&mut self, s: &mut vt100::Screen) {
        let first = base(s);
        let bottom = s.lines_scrolled();
        let last = first + s.line_count() - 1;
        let max_col = cols(s) - 1;
        self.top = self.top.clamp(first, bottom);
        let fix = |p: &mut Pos| {
            p.line = p.line.clamp(first, last);
            p.col = p.col.min(max_col);
        };
        fix(&mut self.cur);
        if let Some(a) = &mut self.anchor {
            fix(a);
        }
        s.set_scrollback(bottom - self.top);
    }

    /// Move the view `by` lines (up when negative), keeping the cursor in it.
    /// Exit when begun by the wheel and back at the bottom with nothing selected.
    pub fn scroll(&mut self, s: &mut vt100::Screen, by: isize) -> Step {
        let bottom = s.lines_scrolled();
        self.top = self.top.saturating_add_signed(by).clamp(base(s), bottom);
        let last_row = self.top + rows(s) - 1;
        self.cur.line = self.cur.line.clamp(self.top, last_row);
        self.apply(s);
        if self.from_wheel && self.top == bottom && self.anchor.is_none() {
            return Step::Exit;
        }
        Step::Stay
    }

    /// Scroll just enough to show the cursor.
    fn follow(&mut self, s: &mut vt100::Screen) {
        let n = rows(s);
        if self.cur.line < self.top {
            self.top = self.cur.line;
        } else if self.cur.line >= self.top + n {
            self.top = self.cur.line + 1 - n;
        }
        self.apply(s);
    }

    fn move_lines(&mut self, s: &mut vt100::Screen, by: isize) {
        let last = base(s) + s.line_count() - 1;
        self.cur.line = self.cur.line.saturating_add_signed(by).clamp(base(s), last);
        self.follow(s);
    }

    /// Move the view and the cursor together (ctrl+u/d, pages).
    fn page(&mut self, s: &mut vt100::Screen, by: isize) {
        let before = self.top;
        self.top = self.top.saturating_add_signed(by).clamp(base(s), s.lines_scrolled());
        let moved = self.top as isize - before as isize;
        // At either end the cursor still goes the whole way.
        let rest = by - moved;
        self.cur.line = self.cur.line.saturating_add_signed(moved);
        self.move_lines(s, rest);
    }

    /// The next (or previous) start of a word, across lines.
    fn word(&mut self, s: &mut vt100::Screen, forward: bool) {
        let first = base(s);
        let last = first + s.line_count() - 1;
        // Characters with their places, from the cursor's line onward (or back).
        let mut id = self.cur.line;
        let (mut text, mut at) = line(s, id);
        let mut chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut i = chars.iter().position(|&(b, _)| at[b] >= self.cur.col).unwrap_or(chars.len());
        let class = |c: char| if c.is_whitespace() { 0 } else if is_word(c) { 1 } else { 2 };
        if forward {
            let start = chars.get(i).map(|&(_, c)| class(c));
            // Skip the rest of this word, then blanks (line ends count as blanks).
            if let Some(k) = start.filter(|&k| k != 0) {
                while chars.get(i).is_some_and(|&(_, c)| class(c) == k) {
                    i += 1;
                }
            }
            loop {
                while chars.get(i).is_some_and(|&(_, c)| class(c) == 0) {
                    i += 1;
                }
                if let Some(&(b, _)) = chars.get(i) {
                    self.cur = Pos { line: id, col: at[b] };
                    break;
                }
                if id >= last {
                    self.cur = Pos { line: id, col: at.last().map_or(0, |&c| c) };
                    break;
                }
                id += 1;
                (text, at) = line(s, id);
                chars = text.char_indices().collect();
                i = 0;
            }
        } else {
            loop {
                // Back over blanks, then to the start of the word before them.
                while i > 0 && class(chars[i - 1].1) == 0 {
                    i -= 1;
                }
                if i > 0 {
                    let k = class(chars[i - 1].1);
                    while i > 0 && class(chars[i - 1].1) == k {
                        i -= 1;
                    }
                    self.cur = Pos { line: id, col: at[chars[i].0] };
                    break;
                }
                if id <= first {
                    self.cur = Pos { line: id, col: 0 };
                    break;
                }
                id -= 1;
                (text, at) = line(s, id);
                chars = text.char_indices().collect();
                i = chars.len();
            }
        }
        self.follow(s);
    }

    /// Find `query` from just past the cursor (or before it), wrapping around.
    fn search(&mut self, s: &mut vt100::Screen, query: &str, forward: bool) -> Step {
        let first = base(s);
        let n = s.line_count();
        let start = self.cur.line - first;
        for step in 0..=n {
            let i = if forward { (start + step) % n } else { (start + n - step % n) % n };
            let id = first + i;
            let (text, at) = line(s, id);
            let hits: Vec<u16> = find_in(&text, query).into_iter().map(|b| at[b]).collect();
            let hit = match (forward, step) {
                (true, 0) => hits.into_iter().find(|&c| c > self.cur.col),
                (false, 0) => hits.into_iter().rev().find(|&c| c < self.cur.col),
                (true, _) if step == n => hits.into_iter().find(|&c| c <= self.cur.col),
                (false, _) if step == n => hits.into_iter().rev().find(|&c| c >= self.cur.col),
                (true, _) => hits.into_iter().next(),
                (false, _) => hits.into_iter().next_back(),
            };
            if let Some(col) = hit {
                self.cur = Pos { line: id, col };
                self.follow(s);
                return Step::Stay;
            }
        }
        Step::Note(format!("not found: {query}"))
    }

    /// The selection as text (the cursor's line when nothing is selected).
    /// Lines that wrapped on screen come back joined.
    pub fn text(&self, s: &vt100::Screen) -> String {
        let (a, b, whole) = match self.anchor {
            Some(an) if an <= self.cur => (an, self.cur, self.whole_lines),
            Some(an) => (self.cur, an, self.whole_lines),
            None => (self.cur, self.cur, true),
        };
        let last = cols(s) - 1;
        let first = base(s);
        let mut out = String::new();
        for id in a.line..=b.line {
            let from = if whole || id != a.line { 0 } else { a.col };
            let to = if whole || id != b.line { last } else { b.col };
            out.push_str(&cut(s, id, from, to));
            let wrapped = to == last && id.checked_sub(first).is_some_and(|i| s.line_wrapped(i));
            if id != b.line && !wrapped {
                out.push('\n');
            }
        }
        out
    }

    pub fn key(&mut self, s: &mut vt100::Screen, k: KeyEvent) -> Step {
        if let Some(p) = &mut self.prompt {
            match k.code {
                KeyCode::Esc => self.prompt = None,
                KeyCode::Enter => {
                    let (q, forward) = (p.input.value().to_string(), p.forward);
                    self.prompt = None;
                    if q.is_empty() {
                        return Step::Stay;
                    }
                    self.last = Some((q.clone(), forward));
                    return self.search(s, &q, forward);
                }
                _ => {
                    p.input.handle(k);
                }
            }
            return Step::Stay;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (rows(s) / 2).max(1) as isize;
        let page = rows(s).saturating_sub(1).max(1) as isize;
        match k.code {
            KeyCode::Char('u') if ctrl => self.page(s, -half),
            KeyCode::Char('d') if ctrl => self.page(s, half),
            KeyCode::Char('b') if ctrl => self.page(s, -page),
            KeyCode::Char('f') if ctrl => self.page(s, page),
            KeyCode::Char('y') if ctrl => return self.scroll(s, -1),
            KeyCode::Char('e') if ctrl => return self.scroll(s, 1),
            KeyCode::Char('c') if ctrl => return Step::Exit,
            KeyCode::PageUp => self.page(s, -page),
            KeyCode::PageDown => self.page(s, page),
            KeyCode::Char('q') => return Step::Exit,
            KeyCode::Esc if self.anchor.is_some() => self.anchor = None,
            KeyCode::Esc => return Step::Exit,
            KeyCode::Char('j') | KeyCode::Down => self.move_lines(s, 1),
            KeyCode::Char('k') | KeyCode::Up => self.move_lines(s, -1),
            KeyCode::Char('h') | KeyCode::Left => self.cur.col = self.cur.col.saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => self.cur.col = (self.cur.col + 1).min(cols(s) - 1),
            KeyCode::Char('0') | KeyCode::Home => self.cur.col = 0,
            KeyCode::Char('$') | KeyCode::End => {
                let (_, at) = line(s, self.cur.line);
                self.cur.col = at.last().copied().unwrap_or(0);
            }
            KeyCode::Char('^') => {
                let (text, at) = line(s, self.cur.line);
                self.cur.col = text.char_indices().find(|(_, c)| !c.is_whitespace()).map_or(0, |(b, _)| at[b]);
            }
            KeyCode::Char('w') => self.word(s, true),
            KeyCode::Char('b') => self.word(s, false),
            KeyCode::Char('g') => {
                self.cur = Pos { line: base(s), col: 0 };
                self.follow(s);
            }
            KeyCode::Char('G') => {
                // The program's cursor line, at the bottom of the view.
                let bottom = s.lines_scrolled();
                self.top = bottom;
                self.cur = Pos { line: bottom + usize::from(s.cursor_position().0), col: 0 };
                self.apply(s);
            }
            KeyCode::Char('H') => self.cur.line = self.top,
            KeyCode::Char('M') => self.cur.line = self.top + rows(s) / 2,
            KeyCode::Char('L') => self.cur.line = self.top + rows(s) - 1,
            KeyCode::Char('v') | KeyCode::Char(' ') => {
                self.anchor = if self.anchor.is_some() && !self.whole_lines { None } else { Some(self.cur) };
                self.whole_lines = false;
            }
            KeyCode::Char('V') => {
                self.anchor = if self.anchor.is_some() && self.whole_lines { None } else { Some(self.cur) };
                self.whole_lines = true;
            }
            KeyCode::Char('y') | KeyCode::Enter => return Step::Copy(self.text(s)),
            KeyCode::Char('/') => self.prompt = Some(Prompt { forward: true, input: TextInput::new("") }),
            KeyCode::Char('?') => self.prompt = Some(Prompt { forward: false, input: TextInput::new("") }),
            KeyCode::Char(c @ ('n' | 'N')) => {
                let Some((q, forward)) = self.last.clone() else { return Step::Note("no search yet: / or ?".into()) };
                return self.search(s, &q, forward == (c == 'n'));
            }
            _ => {}
        }
        self.apply(s);
        Step::Stay
    }

    /// A click (or drag, with `extend`) at view cell (`col`, `row`).
    pub fn point(&mut self, s: &mut vt100::Screen, col: u16, row: u16, extend: bool) {
        let at = Pos { line: self.top + usize::from(row), col };
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cur);
                self.whole_lines = false;
            }
        } else {
            self.anchor = None;
        }
        self.cur = at;
        self.apply(s);
    }
}

/// Put `text` on the clipboard: OSC 52 to the outer terminal, and on Windows
/// and WSL also `clip.exe` (a console in between may drop OSC 52). Never blocks.
pub fn to_clipboard(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
    if cfg!(windows) || std::env::var_os("WSL_DISTRO_NAME").is_some() {
        // clip.exe takes UTF-16LE (UTF-8 comes out as mojibake). No byte
        // order mark: it would stay in the clipboard as a U+FEFF.
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        std::thread::spawn(move || {
            let mut cmd = std::process::Command::new("clip.exe");
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            if let Ok(mut child) = cmd.spawn() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&bytes);
                }
                let _ = child.wait();
            }
        });
    }
}

/// `text` as an OSC 52 sequence that puts it on the clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn screen(rows: u16, cols: u16, text: &str) -> vt100::Parser {
        let mut p = vt100::Parser::new(NonZeroU16::new(rows).unwrap(), NonZeroU16::new(cols).unwrap(), 100);
        p.process(text.as_bytes());
        p
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line{i}\r\n")).collect()
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    fn keys(c: &mut Copy, p: &mut vt100::Parser, ks: &str) -> Step {
        let mut last = Step::Stay;
        for ch in ks.chars() {
            last = c.key(p.screen_mut(), key(ch));
        }
        last
    }

    fn top_text(p: &vt100::Parser) -> String {
        p.screen().contents().lines().next().unwrap_or("").trim_end().to_string()
    }

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo".as_bytes()), "aMOpbGxv");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn moving_up_scrolls_into_history() {
        let mut p = screen(5, 20, &numbered(20));
        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        // The cursor sits on the empty last row; five lines up is line17.
        assert_eq!(c.cursor_cell(p.screen()), Some((0, 4)));
        keys(&mut c, &mut p, "kkkkkkkk");
        assert_eq!(top_text(&p), "line13");
        assert_eq!(c.lines_up(p.screen()), 4);
        assert_eq!(c.cursor_cell(p.screen()), Some((0, 0)));
        keys(&mut c, &mut p, "g");
        assert_eq!(top_text(&p), "line1");
        keys(&mut c, &mut p, "G");
        assert_eq!((top_text(&p).as_str(), c.lines_up(p.screen())), ("line17", 0));
    }

    #[test]
    fn the_view_holds_still_while_output_arrives() {
        let mut p = screen(5, 20, &numbered(20));
        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        keys(&mut c, &mut p, "kkkkkk");
        let before = top_text(&p);
        p.process(b"more\r\nmore\r\nmore\r\n");
        c.apply(p.screen_mut());
        assert_eq!(top_text(&p), before);
        // Even from the bottom: copy mode freezes what it shows.
        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        c.apply(p.screen_mut());
        let before = top_text(&p);
        p.process(b"x\r\ny\r\n");
        c.apply(p.screen_mut());
        assert_eq!(top_text(&p), before);
    }

    #[test]
    fn select_and_copy_characters_and_lines() {
        let mut p = screen(5, 20, "alpha beta\r\ngamma delta\r\n");
        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        // Cursor on row 2 (empty); go up to "gamma delta", then "alpha beta".
        keys(&mut c, &mut p, "kk0w");
        assert_eq!(c.cur.col, 6);
        assert_eq!(keys(&mut c, &mut p, "vjb"), Step::Stay);
        // From "beta" to the start of "gamma" on the next line.
        assert_eq!(c.selected(p.screen(), 0), Some((6, 19)));
        assert_eq!(c.selected(p.screen(), 1), Some((0, 0)));
        assert_eq!(keys(&mut c, &mut p, "y"), Step::Copy("beta\ng".into()));

        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        assert_eq!(keys(&mut c, &mut p, "kkVjy"), Step::Copy("alpha beta\ngamma delta".into()));
        // Nothing selected: the cursor's line.
        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        assert_eq!(keys(&mut c, &mut p, "ky"), Step::Copy("gamma delta".into()));
    }

    #[test]
    fn wrapped_lines_copy_as_one() {
        let mut p = screen(5, 6, "abcdefghij\r\nnext\r\n");
        let mut c = Copy::new("p".into(), (6, 5), 1, p.screen());
        assert_eq!(keys(&mut c, &mut p, "kkkVjjy"), Step::Copy("abcdefghij\nnext".into()));
    }

    #[test]
    fn search_finds_forward_and_back_with_smart_case() {
        let mut p = screen(4, 30, &format!("{}error: one\r\nok\r\nError: two\r\n{}", numbered(10), numbered(3)));
        let mut c = Copy::new("p".into(), (30, 4), 1, p.screen());
        keys(&mut c, &mut p, "?error");
        assert_eq!(c.key(p.screen_mut(), KeyEvent::from(KeyCode::Enter)), Step::Stay);
        let (text, _) = line(p.screen(), c.cur.line);
        assert_eq!(text, "Error: two", "lowercase matches either case");
        keys(&mut c, &mut p, "n");
        assert_eq!(line(p.screen(), c.cur.line).0, "error: one");
        assert!(c.cursor_cell(p.screen()).is_some(), "the view follows");
        keys(&mut c, &mut p, "N");
        assert_eq!(line(p.screen(), c.cur.line).0, "Error: two");
        keys(&mut c, &mut p, "/Nope");
        assert_eq!(c.key(p.screen_mut(), KeyEvent::from(KeyCode::Enter)), Step::Note("not found: Nope".into()));
    }

    #[test]
    fn the_wheel_leaves_at_the_bottom() {
        let mut p = screen(5, 20, &numbered(20));
        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        c.from_wheel = true;
        assert_eq!(c.scroll(p.screen_mut(), -3), Step::Stay);
        assert_eq!(c.lines_up(p.screen()), 3);
        assert_eq!(c.scroll(p.screen_mut(), 3), Step::Exit);
    }

    #[test]
    fn esc_drops_the_selection_first() {
        let mut p = screen(5, 20, &numbered(3));
        let mut c = Copy::new("p".into(), (20, 5), 1, p.screen());
        keys(&mut c, &mut p, "v");
        assert_eq!(c.key(p.screen_mut(), KeyEvent::from(KeyCode::Esc)), Step::Stay);
        assert!(c.anchor.is_none());
        assert_eq!(c.key(p.screen_mut(), KeyEvent::from(KeyCode::Esc)), Step::Exit);
    }

    #[test]
    fn wide_characters_keep_their_columns() {
        let mut p = screen(3, 20, "a日本b x\r\n");
        let (text, at) = line(p.screen(), base(p.screen()));
        assert_eq!(text, "a日本b x");
        assert_eq!(at.iter().copied().collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>(), vec![0, 1, 3, 5, 6, 7]);
        let mut c = Copy::new("p".into(), (20, 3), 1, p.screen());
        keys(&mut c, &mut p, "k0w");
        assert_eq!(c.cur.col, 7, "past the word a日本b");
        let _ = &mut p;
    }
}
