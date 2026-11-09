//! Board state and behaviour, kept free of drawing and I/O so it can be tested.

use crate::git::GitCache;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hc_client::{AgentState, EventData, Terminal, TerminalSet, WatchMsg};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const LABEL_PROJECT: &str = "hiero.project";
pub const LABEL_AGENT: &str = "hiero.agent";

const SHELLS: &[&str] = &["bash", "zsh", "sh", "fish", "nu", "dash", "ksh", "tcsh", "pwsh"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Help,
}

/// Why a terminal wants a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    Blocked,
    Error,
    Done,
    /// Finished on its own (not killed).
    Exited { ok: bool },
}

impl Attention {
    pub fn word(self) -> &'static str {
        match self {
            Attention::Blocked => "blocked",
            Attention::Error => "error",
            Attention::Done => "done",
            Attention::Exited { ok: true } => "exited",
            Attention::Exited { ok: false } => "failed",
        }
    }
}

/// Everything the board shows about one terminal.
#[derive(Debug, Clone)]
pub struct Info {
    pub addr: String,
    pub name: String,
    pub agent: String,
    pub live: bool,
    pub state: AgentState,
    pub status: String,
    pub status_msg: String,
    pub attention: Option<Attention>,
    pub project: String,
    pub branch: Option<String>,
    pub dir: String,
    pub up_ms: u64,
    /// How long it has waited for a person (needs-you only).
    pub wait_ms: Option<u64>,
    pub title: String,
    pub turn: u64,
}

#[derive(Debug, Clone)]
pub enum Entry {
    Header { title: String, count: usize, attention: bool },
    Item(Box<Info>),
}

#[derive(Debug, Default)]
pub struct Preview {
    /// Whose text this is.
    pub addr: Option<String>,
    pub text: String,
    pub error: Option<String>,
    /// Lines scrolled up from the bottom.
    pub scroll: u16,
    pub dirty: bool,
    pub loading: bool,
}

pub struct Alert {
    pub title: String,
    pub body: String,
}

pub struct App {
    pub set: TerminalSet,
    pub core_name: String,
    /// Why we're not connected (None when fine).
    pub disconnected: Option<String>,
    pub connected_once: bool,
    pub selected: Option<String>,
    pub mode: Mode,
    pub query: String,
    pub group_by_project: bool,
    pub show_finished: bool,
    /// Address → the attention timestamp the person dismissed.
    pub dismissed: HashMap<String, u64>,
    pub preview: Preview,
    pub entries: Vec<Entry>,
    pub git: GitCache,
    pub flash: Option<(String, Instant)>,
    pub tick: u64,
    pub quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        App {
            set: TerminalSet::new(),
            core_name: String::new(),
            disconnected: None,
            connected_once: false,
            selected: None,
            mode: Mode::Normal,
            query: String::new(),
            group_by_project: true,
            show_finished: true,
            dismissed: HashMap::new(),
            preview: Preview::default(),
            entries: Vec::new(),
            git: GitCache::default(),
            flash: None,
            tick: 0,
            quit: false,
        }
    }

    // ---- incoming -----------------------------------------------------------

    /// Fold a watcher message in; returns alerts to show.
    pub fn on_watch(&mut self, msg: &WatchMsg) -> Vec<Alert> {
        let mut alerts = Vec::new();
        match msg {
            WatchMsg::Snapshot { .. } => {
                self.set.apply_watch(msg);
                self.disconnected = None;
                self.connected_once = true;
                self.preview.dirty = true;
            }
            WatchMsg::Disconnected { error } => {
                self.set.apply_watch(msg);
                self.disconnected = Some(error.clone());
            }
            WatchMsg::Event(e) => {
                let addr = e.terminal.clone();
                let before = addr.as_deref().and_then(|a| self.set.get(a)).and_then(attention);
                self.set.apply(e);
                if let Some(a) = addr.as_deref() {
                    if let Some(t) = self.set.get(a) {
                        let after = attention(t);
                        if let Some(now) = after
                            && Some(now) != before
                        {
                            alerts.push(alert_for(t, now));
                        }
                    }
                    let affects_screen = !matches!(
                        e.data,
                        EventData::Foreground(_) | EventData::Labels { .. } | EventData::Keep { .. }
                    );
                    if affects_screen && self.selected.as_deref() == Some(a) {
                        self.preview.dirty = true;
                    }
                }
            }
        }
        self.rebuild();
        alerts
    }

    pub fn on_preview(&mut self, addr: String, result: Result<String, String>) {
        self.preview.loading = false;
        if self.selected.as_deref() != Some(addr.as_str()) {
            self.preview.dirty = true; // selection moved on meanwhile
            return;
        }
        if self.preview.addr.as_deref() != Some(addr.as_str()) {
            self.preview.scroll = 0;
        }
        self.preview.addr = Some(addr);
        match result {
            Ok(text) => {
                self.preview.text = text.trim_end().to_string();
                self.preview.error = None;
            }
            Err(e) => self.preview.error = Some(e),
        }
    }

    /// The terminal whose screen should be fetched now, if any. Marks it loading.
    pub fn take_preview_request(&mut self) -> Option<String> {
        if !self.preview.dirty || self.preview.loading || self.disconnected.is_some() {
            return None;
        }
        let addr = self.selected.clone()?;
        self.preview.dirty = false;
        self.preview.loading = true;
        Some(addr)
    }

    /// Called about once a second: live screens change without events.
    pub fn on_slow_tick(&mut self) {
        if let Some(t) = self.selected.as_deref().and_then(|a| self.set.get(a))
            && t.is_live()
        {
            self.preview.dirty = true;
        }
        if let Some((_, at)) = &self.flash
            && at.elapsed().as_secs() >= 3
        {
            self.flash = None;
        }
    }

    // ---- keys ---------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Normal,
            Mode::Search => match key.code {
                KeyCode::Esc => {
                    self.query.clear();
                    self.mode = Mode::Normal;
                }
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Down => self.move_by(1),
                KeyCode::Up => self.move_by(-1),
                KeyCode::Char(c) => self.query.push(c),
                _ => {}
            },
            Mode::Normal => match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
                KeyCode::Char('g') | KeyCode::Home => self.move_to_edge(false),
                KeyCode::Char('G') | KeyCode::End => self.move_to_edge(true),
                KeyCode::Char('n') | KeyCode::Tab => self.next_attention(),
                KeyCode::Char('/') => self.mode = Mode::Search,
                KeyCode::Esc => self.query.clear(),
                KeyCode::Char('p') => {
                    self.group_by_project = !self.group_by_project;
                    self.flash(if self.group_by_project { "grouped by project" } else { "one list" });
                }
                KeyCode::Char('f') => {
                    self.show_finished = !self.show_finished;
                    self.flash(if self.show_finished { "showing finished" } else { "hiding finished" });
                }
                KeyCode::Char('x') => self.dismiss_selected(),
                KeyCode::Char('r') => self.preview.dirty = true,
                KeyCode::Char('J') | KeyCode::PageDown => {
                    self.preview.scroll = self.preview.scroll.saturating_sub(10)
                }
                KeyCode::Char('K') | KeyCode::PageUp => {
                    self.preview.scroll = self.preview.scroll.saturating_add(10)
                }
                _ => {}
            },
        }
        self.rebuild();
    }

    fn flash(&mut self, msg: &str) {
        self.flash = Some((msg.to_string(), Instant::now()));
    }

    fn item_addrs(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item(i) => Some(i.addr.as_str()),
                _ => None,
            })
            .collect()
    }

    fn select(&mut self, addr: Option<String>) {
        if addr != self.selected {
            self.selected = addr;
            self.preview.dirty = true;
            self.preview.scroll = 0;
        }
    }

    fn move_by(&mut self, delta: isize) {
        let items = self.item_addrs();
        if items.is_empty() {
            return;
        }
        let cur = self
            .selected
            .as_deref()
            .and_then(|s| items.iter().position(|a| *a == s))
            .unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, items.len() as isize - 1) as usize;
        let addr = items[next].to_string();
        self.select(Some(addr));
    }

    fn move_to_edge(&mut self, end: bool) {
        let items = self.item_addrs();
        let addr = if end { items.last() } else { items.first() }.map(|s| s.to_string());
        self.select(addr);
    }

    fn next_attention(&mut self) {
        let waiting: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item(i) if i.wait_ms.is_some() => Some(i.addr.clone()),
                _ => None,
            })
            .collect();
        if waiting.is_empty() {
            self.flash("nobody needs you");
            return;
        }
        let pos = self.selected.as_ref().and_then(|s| waiting.iter().position(|a| a == s));
        let next = match pos {
            Some(p) => waiting[(p + 1) % waiting.len()].clone(),
            None => waiting[0].clone(),
        };
        self.select(Some(next));
    }

    fn dismiss_selected(&mut self) {
        let Some(t) = self.selected.as_deref().and_then(|a| self.set.get(a)) else {
            return;
        };
        if attention(t).is_some() {
            let (addr, ts) = (t.address.clone(), attention_ts(t));
            self.dismissed.insert(addr, ts);
            self.flash("dismissed");
        }
    }

    // ---- view ---------------------------------------------------------------

    /// Recompute the rows. Cheap enough to call on every change and frame.
    pub fn rebuild(&mut self) {
        let now = now_ms();
        let query = self.query.to_lowercase();
        let mut waiting: Vec<Info> = Vec::new();
        let mut groups: BTreeMap<String, Vec<Info>> = BTreeMap::new();

        let terms: Vec<Terminal> = self.set.terminals.values().cloned().collect();
        for t in &terms {
            let info = self.info(t, now);
            if !query.is_empty() && !fuzzy_match(&query, &haystack(&info)) {
                continue;
            }
            if info.wait_ms.is_some() {
                waiting.push(info);
            } else if info.live || self.show_finished {
                let key = if self.group_by_project { info.project.clone() } else { "agents".into() };
                groups.entry(key).or_default().push(info);
            }
        }

        waiting.sort_by_key(|i| std::cmp::Reverse(i.wait_ms));
        let mut entries = Vec::new();
        if !waiting.is_empty() {
            entries.push(Entry::Header {
                title: "needs you".into(),
                count: waiting.len(),
                attention: true,
            });
            entries.extend(waiting.into_iter().map(|i| Entry::Item(Box::new(i))));
        }
        for (title, mut items) in groups {
            items.sort_by(|a, b| (!a.live, b.up_ms).cmp(&(!b.live, a.up_ms)));
            entries.push(Entry::Header {
                title,
                count: items.len(),
                attention: false,
            });
            entries.extend(items.into_iter().map(|i| Entry::Item(Box::new(i))));
        }
        self.entries = entries;

        // Keep the selection on a visible row.
        let items = self.item_addrs();
        let keep = self.selected.as_deref().is_some_and(|s| items.contains(&s));
        if !keep {
            let first = items.first().map(|s| s.to_string());
            self.select(first);
        }
    }

    pub fn attention_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::Item(i) if i.wait_ms.is_some()))
            .count()
    }

    pub fn selected_info(&self) -> Option<&Info> {
        self.entries.iter().find_map(|e| match e {
            Entry::Item(i) if Some(&i.addr) == self.selected.as_ref() => Some(&**i),
            _ => None,
        })
    }

    fn info(&mut self, t: &Terminal, now: u64) -> Info {
        let dir = t
            .foreground
            .as_ref()
            .map(|f| f.cwd.as_str())
            .filter(|d| !d.is_empty())
            .unwrap_or(&t.cwd)
            .to_string();
        let git = self.git.lookup(&dir);
        let project = t
            .labels
            .get(LABEL_PROJECT)
            .cloned()
            .or_else(|| git.as_ref().map(|g| g.name()))
            .unwrap_or_else(|| basename(&dir));

        let att = attention(t);
        let dismissed = self.dismissed.get(&t.address) == Some(&attention_ts(t));
        let wait_ms = match att {
            Some(_) if !dismissed => Some(now.saturating_sub(attention_ts(t))),
            _ => None,
        };
        let end = t.finished_ms.unwrap_or(now);
        Info {
            addr: t.address.clone(),
            name: t.display_name().to_string(),
            agent: agent_name(t),
            live: t.is_live(),
            state: t.agent_state(),
            status: status_word(t),
            status_msg: status_msg(t),
            attention: att,
            project,
            branch: git.map(|g| g.branch),
            dir,
            up_ms: end.saturating_sub(t.created_ms),
            wait_ms,
            title: t.title.clone(),
            turn: t.turn,
        }
    }
}

// ---- helpers ----------------------------------------------------------------

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Why this terminal wants a person, if it does.
pub fn attention(t: &Terminal) -> Option<Attention> {
    if t.is_live() {
        return match t.agent_state() {
            AgentState::Blocked => Some(Attention::Blocked),
            AgentState::Error => Some(Attention::Error),
            AgentState::Done => Some(Attention::Done),
            _ => None,
        };
    }
    match t.reason.as_deref() {
        Some("killed" | "server_stop" | "host_shutdown" | "stopped_by_restart") => None,
        _ => Some(Attention::Exited {
            ok: t.exit_code == Some(0),
        }),
    }
}

/// When it started wanting a person; identifies one occurrence for dismissal.
fn attention_ts(t: &Terminal) -> u64 {
    if t.is_live() {
        t.status_ms.unwrap_or(t.created_ms)
    } else {
        t.finished_ms.unwrap_or(t.created_ms)
    }
}

fn alert_for(t: &Terminal, a: Attention) -> Alert {
    let mut body = format!("{} is {}", t.display_name(), a.word());
    if let Some(s) = &t.status {
        if let Some(kind) = &s.kind {
            body.push_str(&format!(" ({kind})"));
        }
        if !s.msg.is_empty() {
            body.push_str(&format!(": {}", s.msg));
        }
    }
    Alert {
        title: format!("hiero · {}", agent_name(t)),
        body,
    }
}

pub fn agent_name(t: &Terminal) -> String {
    if let Some(a) = t.labels.get(LABEL_AGENT) {
        return a.clone();
    }
    if let Some(tpl) = &t.template {
        return tpl.clone();
    }
    if let Some(app) = t.status.as_ref().map(|s| s.app.as_str()).filter(|a| !a.is_empty()) {
        return app.to_string();
    }
    if let Some(fg) = &t.foreground
        && !SHELLS.contains(&fg.name.as_str())
    {
        return fg.name.clone();
    }
    t.argv
        .first()
        .map(|a| basename(a))
        .unwrap_or_else(|| "shell".into())
}

fn status_word(t: &Terminal) -> String {
    if !t.is_live() {
        return match (t.reason.as_deref(), t.exit_code) {
            (Some("killed"), _) => "killed".into(),
            (_, Some(0)) => "exited".into(),
            (_, Some(c)) => format!("exit {c}"),
            (Some(r), None) => r.replace('_', " "),
            _ => "finished".into(),
        };
    }
    match &t.status {
        Some(s) => s.state.clone(),
        None => "–".into(),
    }
}

/// The agent's own message, prefixed with why it's blocked when it said.
fn status_msg(t: &Terminal) -> String {
    let Some(s) = &t.status else { return String::new() };
    match (&s.kind, s.msg.is_empty()) {
        (Some(k), true) => format!("[{k}]"),
        (Some(k), false) => format!("[{k}] {}", s.msg),
        (None, _) => s.msg.clone(),
    }
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

fn haystack(i: &Info) -> String {
    format!(
        "{} {} {} {} {} {} {}",
        i.name,
        i.addr,
        i.agent,
        i.status,
        i.project,
        i.branch.as_deref().unwrap_or(""),
        i.title
    )
    .to_lowercase()
}

/// Every character of `needle` appears in `hay` in order.
pub fn fuzzy_match(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().filter(|c| !c.is_whitespace()).all(|c| it.any(|h| h == c))
}

/// 42s, 5m, 1h05m, 3d.
pub fn fmt_dur(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_client::{Event, Status};

    fn term(addr: &str, name: &str) -> Terminal {
        serde_json::from_value(serde_json::json!({
            "address": addr, "name": name, "labels": {}, "argv": ["claude"], "cwd": "/tmp",
            "state": "live", "created_ms": 1000, "screen": "", "cols": 80, "rows": 24
        }))
        .unwrap()
    }

    fn status_event(seq: u64, addr: &str, state: &str) -> WatchMsg {
        WatchMsg::Event(Event {
            seq,
            terminal: Some(addr.into()),
            data: EventData::Status {
                status: Some(Status {
                    state: state.into(),
                    app: String::new(),
                    kind: None,
                    msg: String::new(),
                    progress: None,
                    title: String::new(),
                }),
                records: Default::default(),
                reported: true,
            },
        })
    }

    fn app_with(terms: Vec<Terminal>) -> App {
        let mut app = App::new();
        app.on_watch(&WatchMsg::Snapshot { seq: 1, terminals: terms });
        app
    }

    #[test]
    fn blocked_agent_moves_to_needs_you_and_alerts_once() {
        let mut app = app_with(vec![term("t1", "alpha"), term("t2", "beta")]);
        assert_eq!(app.attention_count(), 0);

        let alerts = app.on_watch(&status_event(2, "t2", "working"));
        assert!(alerts.is_empty());

        let alerts = app.on_watch(&status_event(3, "t2", "blocked"));
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].body.contains("beta is blocked"));
        assert_eq!(app.attention_count(), 1);
        assert!(matches!(&app.entries[0], Entry::Header { attention: true, .. }));
        assert!(matches!(&app.entries[1], Entry::Item(i) if i.addr == "t2"));

        // Same state again: no repeat alert.
        assert!(app.on_watch(&status_event(4, "t2", "blocked")).is_empty());
    }

    #[test]
    fn dismiss_hides_until_next_occurrence() {
        let mut t = term("t1", "alpha");
        t.status = Some(Status {
            state: "done".into(),
            app: String::new(),
            kind: None,
            msg: String::new(),
            progress: None,
            title: String::new(),
        });
        t.status_ms = Some(5000);
        let mut app = app_with(vec![t]);
        assert_eq!(app.attention_count(), 1);
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        assert_eq!(app.attention_count(), 0);
    }

    #[test]
    fn search_filters_and_keeps_selection_valid() {
        let mut app = app_with(vec![term("t1", "alpha"), term("t2", "beta")]);
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "bta".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        let items: Vec<_> = app.item_addrs().into_iter().map(String::from).collect();
        assert_eq!(items, vec!["t2"]);
        assert_eq!(app.selected.as_deref(), Some("t2"));
    }

    #[test]
    fn navigation_skips_headers() {
        let mut app = app_with(vec![term("t1", "a"), term("t2", "b")]);
        let first = app.selected.clone();
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert_ne!(app.selected, first);
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(app.selected, first);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_dur(42_000), "42s");
        assert_eq!(fmt_dur(65 * 60_000), "1h05m");
        assert!(fuzzy_match("bta", "beta"));
        assert!(!fuzzy_match("x", "beta"));
    }
}
