//! State and key handling of the workspace screen: the sidebar, the prefix
//! key, dialogs, and which terminal the pane shows. No I/O here: the event
//! loop (`ws_tui`) carries out the [`Action`]s this asks for, so all of it
//! can be tested.

use crate::app::{Alert, BoardStart, PaneView, status_from_labels};
use crate::config::Prefix;
use crate::forms::{TextInput, expand_home};
use crate::workspace::{self, Dir, Move, Tab, Workspace, WsDoc, WsOp, WsStatus, new_id};
use crate::ws_ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hc_client::{EventData, RunParams, Terminal, TerminalSet, WatchMsg};
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub const LABEL_WS: &str = "hiero.ws";
pub const LABEL_PANE: &str = "hiero.pane";
pub const LABEL_PROJECT: &str = "hiero.project";

/// How much one `H` `J` `K` `L` press moves a border, in percent of the split.
const RESIZE_STEP: i16 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Workspace,
    Sidebar,
}

/// One visible line of the sidebar, by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Project(String),
    Workspace(String),
}

impl Row {
    pub fn id(&self) -> &str {
        match self {
            Row::Project(id) | Row::Workspace(id) => id,
        }
    }
}

/// How to fill the pane of a workspace.
#[derive(Debug, Clone)]
pub enum Plan {
    /// The pane's terminal is still running: attach to it.
    Attach { pane: String, addr: String },
    /// There is none (never started, or gone): start one, then record it.
    Start { pane: String, params: RunParams },
    /// Its program ended on its own: show that, and start nothing.
    Ended { pane: String, addr: String, why: String },
}

impl Plan {
    pub fn pane(&self) -> &str {
        match self {
            Plan::Attach { pane, .. } | Plan::Start { pane, .. } | Plan::Ended { pane, .. } => pane,
        }
    }
}

/// Work for the event loop. (Short-lived and few, so the size gap is fine.)
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Action {
    LoadDoc,
    Update(Vec<WsOp>),
    /// Close the pane's attach stream (the program keeps running).
    Detach(String),
    /// Start or attach, then stream into a pane of this many inner cells.
    /// `attempt` tells a late answer from one for an older attempt.
    Open { plan: Plan, attempt: u64, cols: u16, rows: u16 },
    Resize { pane: String, cols: u16, rows: u16 },
    /// Kill these terminals and remove them from hc.
    Kill(Vec<String>),
    /// Start a pane's terminal in the background (no attach) and record it.
    Start { pane: String, params: RunParams },
    /// Hand the screen to the old board, opened on this view.
    Board(BoardStart),
}

/// What a pane's terminal needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fate {
    /// Still running.
    Running,
    /// Not there, or taken down by the system: start it again.
    Missing,
    /// Its program ended on its own or was killed: leave it, say why.
    Ended(String),
}

/// The restart rule. A terminal that is gone from hc, or that finished
/// because the system went down (a reboot, a core restart), comes back by
/// itself. One that exited or was killed was ended on purpose, so it stays
/// ended until the person asks.
pub fn fate(t: Option<&Terminal>) -> Fate {
    let Some(t) = t else { return Fate::Missing };
    if t.is_live() {
        return Fate::Running;
    }
    match t.reason.as_deref() {
        Some("server_stop" | "host_shutdown" | "holder_lost" | "stopped_by_restart") => Fate::Missing,
        reason => {
            let why = reason.unwrap_or("ended").replace('_', " ");
            Fate::Ended(match t.exit_code {
                Some(code) => format!("{why} (code {code})"),
                None => why,
            })
        }
    }
}

/// How to fill one pane. With `fresh` a new terminal is started even if the
/// old one still runs.
pub fn plan(doc: &WsDoc, set: &TerminalSet, ws_id: &str, pane_id: &str, fresh: bool) -> Option<Plan> {
    let (project, w) = doc.workspace(ws_id)?;
    let pane = w.panes().into_iter().find(|p| p.id == pane_id)?;
    if !fresh && let Some(addr) = &pane.terminal {
        match fate(set.get(addr)) {
            Fate::Running => return Some(Plan::Attach { pane: pane.id.clone(), addr: addr.clone() }),
            Fate::Ended(why) => return Some(Plan::Ended { pane: pane.id.clone(), addr: addr.clone(), why }),
            Fate::Missing => {}
        }
    }
    let mut params = RunParams::new().cwd(pane.cwd.clone().unwrap_or_else(|| w.folder.clone()));
    if let Some(cmd) = &pane.command {
        params = params.line(cmd.clone());
    }
    params.labels.insert(LABEL_WS.into(), w.id.clone());
    params.labels.insert(LABEL_PANE.into(), pane.id.clone());
    params.labels.insert(LABEL_PROJECT.into(), project.name.clone());
    Some(Plan::Start { pane: pane.id.clone(), params })
}

/// A direction for moving the focus between panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Left,
    Right,
    Up,
    Down,
}

/// The pane next to `from` in direction `nav`, by where the panes really are:
/// the nearest ones that overlap it along the other axis, the biggest overlap
/// first (the first one, top or left, on a tie).
pub fn neighbor(rects: &[(String, Rect)], from: &str, nav: Nav) -> Option<String> {
    let me = rects.iter().find(|(id, _)| id == from)?.1;
    let span = |r: Rect, vertical_axis: bool| -> (i32, i32) {
        if vertical_axis { (r.y as i32, r.y as i32 + r.height as i32) } else { (r.x as i32, r.x as i32 + r.width as i32) }
    };
    let (mut best, mut best_key) = (None, (i32::MAX, 0));
    for (id, r) in rects.iter().filter(|(id, _)| id != from) {
        // `gap` along the move axis; `lo..hi` overlap on the other.
        let (gap, (a0, a1), (b0, b1)) = match nav {
            Nav::Right => (r.x as i32 - (me.x as i32 + me.width as i32), span(me, true), span(*r, true)),
            Nav::Left => (me.x as i32 - (r.x as i32 + r.width as i32), span(me, true), span(*r, true)),
            Nav::Down => (r.y as i32 - (me.y as i32 + me.height as i32), span(me, false), span(*r, false)),
            Nav::Up => (me.y as i32 - (r.y as i32 + r.height as i32), span(me, false), span(*r, false)),
        };
        let overlap = a1.min(b1) - a0.max(b0);
        if gap < 0 || overlap <= 0 {
            continue;
        }
        let key = (gap, -overlap);
        if best.is_none() || key < best_key {
            best = Some(id.clone());
            best_key = key;
        }
    }
    best
}

// ---- dialogs ----------------------------------------------------------------

pub enum FormKind {
    AddProject,
    AddWorkspace { project: String },
    Rename { id: String },
    RenameTab { id: String },
    /// A new tab, asking what it runs first.
    NewTab { workspace: String },
    /// A split of `pane`, asking what the new pane runs first.
    NewSplit { pane: String, dir: Dir },
}

/// What a yes/no question is about.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmKind {
    Delete(String),
    ClosePane(String),
    CloseTab(String),
}

pub struct Field {
    pub label: &'static str,
    pub input: TextInput,
}

pub struct Form {
    pub kind: FormKind,
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
}

pub enum Dialog {
    /// The help text, scrolled by this many lines.
    Help(usize),
    Form(Form),
    Confirm { kind: ConfirmKind, message: String },
    /// The menu of the old views; the index is the cursor.
    Menu(usize),
}

/// The menu entries, in order.
pub const MENU: [(&str, BoardStart); 6] = [
    ("Agents board", BoardStart::Board),
    ("Tasks", BoardStart::Tasks),
    ("Pipelines", BoardStart::Pipelines),
    ("Usage", BoardStart::Usage),
    ("Templates", BoardStart::Templates),
    ("Broadcast", BoardStart::Broadcast),
];

/// What a key press means for the program in the pane.
#[derive(Debug, PartialEq)]
pub enum Out {
    Key(KeyEvent),
    /// Raw bytes (the prefix key typed twice).
    Bytes(Vec<u8>),
}

/// One pane of the active tab: its stream's screen, or why it has none.
pub struct Slot {
    pub attempt: u64,
    /// Present once attached.
    pub view: Option<PaneView>,
    pub error: Option<String>,
    /// Not started on purpose (`restore = false`): Enter starts it.
    pub idle: bool,
    /// Inner cells (cols, rows) the stream and the parser have.
    pub size: (u16, u16),
}

/// A short error in the bottom bar.
struct Flash {
    msg: String,
    error: bool,
    at: Instant,
}

pub struct WsApp {
    pub doc: WsDoc,
    pub set: TerminalSet,
    pub focus: Focus,
    /// The sidebar cursor: a project or workspace id.
    pub sel: String,
    pub prefix: Prefix,
    /// The prefix was pressed; the next key is a command.
    pub armed: bool,
    pub dialog: Option<Dialog>,
    /// The workspace shown in the pane area.
    pub open: Option<String>,
    /// The panes of the active tab that are open or opening, by pane id.
    pub slots: HashMap<String, Slot>,
    /// Panes whose terminal is being started right now (never start one twice).
    starting: HashSet<String>,
    /// Terminals started in this run, by pane: the terminal list may not
    /// know them yet, and they must not be started a second time.
    recent: HashMap<String, String>,
    /// Background starts that failed; they are not retried until the pane is opened.
    failed: HashSet<String>,
    /// `[workspace] restore`: start missing terminals without being asked.
    pub auto_restore: bool,
    next_attempt: u64,
    /// The tab (id) whose focused pane fills the area. View only, not saved.
    pub zoom: Option<String>,
    /// Where the panes go (below the tabs bar).
    pub panes_area: Rect,
    /// Height of the whole screen, for the scrolling help.
    screen_h: u16,
    pub actions: Vec<Action>,
    pub disconnected: Option<String>,
    pub connected_once: bool,
    pub quit: bool,
    pub launch_dir: String,
    flash: Option<Flash>,
    last_status: HashMap<String, WsStatus>,
    synced: bool,
    loaded: bool,
    /// The workspace to reopen once the document is first read.
    restore: Option<String>,
    /// Select / open these as soon as the document that holds them arrives.
    pending_sel: Option<String>,
    pending_open: Option<String>,
}

impl WsApp {
    pub fn new(prefix: Prefix, restore: Option<String>) -> WsApp {
        WsApp {
            doc: WsDoc::default(),
            set: TerminalSet::new(),
            focus: Focus::Sidebar,
            sel: String::new(),
            prefix,
            armed: false,
            dialog: None,
            open: None,
            slots: HashMap::new(),
            starting: HashSet::new(),
            recent: HashMap::new(),
            failed: HashSet::new(),
            auto_restore: true,
            next_attempt: 0,
            zoom: None,
            panes_area: ws_ui::areas(Rect::new(0, 0, 100, 30), false).panes,
            screen_h: 30,
            actions: Vec::new(),
            disconnected: None,
            connected_once: false,
            quit: false,
            launch_dir: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            flash: None,
            last_status: HashMap::new(),
            synced: false,
            loaded: false,
            restore,
            pending_sel: None,
            pending_open: None,
        }
    }

    // ---- sidebar rows ---------------------------------------------------------

    /// Projects and, unless collapsed, their workspaces, in doc order.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for p in &self.doc.projects {
            rows.push(Row::Project(p.id.clone()));
            if !p.collapsed {
                rows.extend(p.workspaces.iter().map(|w| Row::Workspace(w.id.clone())));
            }
        }
        rows
    }

    fn sel_index(&self) -> Option<usize> {
        self.rows().iter().position(|r| r.id() == self.sel)
    }

    pub fn selected(&self) -> Option<Row> {
        self.rows().into_iter().find(|r| r.id() == self.sel)
    }

    /// The project the cursor is in (or the open workspace's, with no cursor).
    fn current_project(&self) -> Option<String> {
        if let Some((p, _)) = self.doc.workspace(&self.sel) {
            return Some(p.id.clone());
        }
        if self.doc.project(&self.sel).is_some() {
            return Some(self.sel.clone());
        }
        self.open.as_deref().and_then(|w| self.doc.workspace(w)).map(|(p, _)| p.id.clone())
    }

    fn move_sel(&mut self, by: i32) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let i = self.sel_index().unwrap_or(0) as i32;
        let j = (i + by).clamp(0, rows.len() as i32 - 1) as usize;
        self.sel = rows[j].id().to_string();
    }

    /// Keep the cursor on a visible row after the document changed.
    fn fix_sel(&mut self) {
        if self.sel_index().is_some() {
            return;
        }
        // Hidden inside a collapsed project: land on the project.
        if let Some((p, _)) = self.doc.workspace(&self.sel) {
            self.sel = p.id.clone();
            return;
        }
        self.sel = self.rows().first().map(|r| r.id().to_string()).unwrap_or_default();
    }

    // ---- status -------------------------------------------------------------

    pub fn ws_status(&self, id: &str) -> WsStatus {
        self.doc.workspace(id).map(|(_, w)| workspace::workspace_status(w, &self.set)).unwrap_or(WsStatus::None)
    }

    pub fn project_status(&self, id: &str) -> WsStatus {
        self.doc.project(id).map(|p| workspace::project_status(p, &self.set)).unwrap_or(WsStatus::None)
    }

    /// The first workspace that needs you, as (project, workspace) names.
    pub fn attention(&self) -> Option<(String, String)> {
        self.doc.projects.iter().find_map(|p| {
            p.workspaces
                .iter()
                .find(|w| workspace::workspace_status(w, &self.set) == WsStatus::Attention)
                .map(|w| (p.name.clone(), w.name.clone()))
        })
    }

    fn statuses(&self) -> HashMap<String, WsStatus> {
        self.doc
            .projects
            .iter()
            .flat_map(|p| &p.workspaces)
            .map(|w| (w.id.clone(), workspace::workspace_status(w, &self.set)))
            .collect()
    }

    /// Alerts for workspaces that just moved into attention or done.
    fn status_alerts(&mut self) -> Vec<Alert> {
        let now = self.statuses();
        let mut alerts = Vec::new();
        if self.synced {
            for (id, s) in &now {
                let before = self.last_status.get(id).copied().unwrap_or(WsStatus::None);
                if *s == before {
                    continue;
                }
                let Some((p, w)) = self.doc.workspace(id) else { continue };
                let (what, body) = match s {
                    WsStatus::Attention => ("blocked", "needs you"),
                    WsStatus::Done => ("done", "is done"),
                    _ => continue,
                };
                alerts.push(Alert {
                    what: what.into(),
                    title: format!("{} › {}", p.name, w.name),
                    body: format!("{} › {} {body}", p.name, w.name),
                });
            }
        }
        self.last_status = now;
        alerts
    }

    // ---- incoming -------------------------------------------------------------

    pub fn on_watch(&mut self, msg: &WatchMsg) -> Vec<Alert> {
        match msg {
            WatchMsg::Snapshot { .. } => {
                self.set.apply_watch(msg);
                self.set.terminals.values_mut().for_each(status_from_labels);
                self.disconnected = None;
                self.connected_once = true;
                // A fresh snapshot is a new baseline, not a change.
                self.last_status = self.statuses();
                self.synced = true;
                // Only now do we know which terminals exist, so only now is
                // it safe to decide what is missing.
                self.sync();
                Vec::new()
            }
            WatchMsg::Disconnected { error } => {
                self.set.apply_watch(msg);
                self.disconnected = Some(error.clone());
                Vec::new()
            }
            WatchMsg::Event(e) => {
                if let EventData::Doc { name, .. } = &e.data
                    && name == workspace::DOC_NAME
                {
                    self.actions.push(Action::LoadDoc);
                }
                self.set.apply(e);
                if let Some(t) = e.terminal.as_deref().and_then(|a| self.set.terminals.get_mut(a)) {
                    status_from_labels(t);
                }
                self.status_alerts()
            }
        }
    }

    /// A fresh copy of the document (first read, reload, or after our own edit).
    pub fn on_doc(&mut self, doc: WsDoc) {
        self.doc = doc;
        if let Some(id) = self.pending_sel.take_if(|id| self.doc.project(id).is_some()) {
            self.sel = id;
        }
        self.fix_sel();
        if !self.loaded {
            self.loaded = true;
            match self.restore.take().filter(|id| self.doc.workspace(id).is_some()) {
                Some(id) => {
                    self.sel = id.clone();
                    self.focus = Focus::Workspace;
                    self.open_workspace(&id);
                }
                None => self.focus = Focus::Sidebar,
            }
        }
        if let Some(id) = self.pending_open.take_if(|id| self.doc.workspace(id).is_some()) {
            self.sel = id.clone();
            self.focus = Focus::Workspace;
            self.open_workspace(&id);
        }
        // The open workspace was deleted (here or from the CLI).
        if let Some(id) = &self.open
            && self.doc.workspace(id).is_none()
        {
            self.open = None;
        }
        self.fix_sel();
        self.sync();
    }

    // ---- the panes ------------------------------------------------------------

    /// The open workspace and its active tab.
    pub fn active(&self) -> Option<(&Workspace, &Tab)> {
        let (_, w) = self.doc.workspace(self.open.as_deref()?)?;
        let t = w.tabs.get(w.active_tab).or(w.tabs.first())?;
        Some((w, t))
    }

    /// The pane of the active tab that has the keyboard.
    pub fn focused_pane(&self) -> Option<String> {
        let (_, t) = self.active()?;
        let p = t.layout.find(&t.focus).or_else(|| t.layout.panes().into_iter().next())?;
        Some(p.id.clone())
    }

    pub fn zoomed(&self) -> bool {
        self.active().is_some_and(|(_, t)| self.zoom.as_deref() == Some(t.id.as_str()))
    }

    /// Where the panes of the active tab are drawn (outer cells, borders
    /// included). Zoomed: only the focused pane, over the whole area.
    pub fn pane_rects(&self) -> Vec<(String, Rect)> {
        let Some((_, t)) = self.active() else { return vec![] };
        if self.zoomed()
            && let Some(f) = self.focused_pane()
        {
            return vec![(f, self.panes_area)];
        }
        workspace::rects(&t.layout, self.panes_area)
    }

    /// The cells the program gets inside a pane's border.
    fn inner(r: Rect) -> (u16, u16) {
        (r.width.saturating_sub(2).max(1), r.height.saturating_sub(2).max(1))
    }

    fn size_of(&self, pane: &str) -> (u16, u16) {
        let r = self.pane_rects().into_iter().find(|(id, _)| id == pane).map(|(_, r)| r);
        Self::inner(r.unwrap_or(self.panes_area))
    }

    /// The screen goes to the board for a while: drop every pane's stream
    /// state. `sync` opens them again (attaching to the same terminals).
    pub fn suspend(&mut self) {
        self.slots.clear();
        self.zoom = None;
    }

    /// The screen changed size (or the banner came or went).
    pub fn set_screen(&mut self, screen: Rect) {
        self.panes_area = ws_ui::areas(screen, self.disconnected.is_some()).panes;
        self.screen_h = screen.height;
        self.sync();
    }

    /// Make the open panes match what the document says is on screen: close
    /// the ones that left, open the new ones (attach if the terminal still
    /// runs, else start one), and resize the ones whose rectangle changed.
    /// Safe to call at any time; it only acts on differences.
    pub fn sync(&mut self) {
        // Before the first snapshot every terminal looks missing.
        if !self.synced {
            return;
        }
        let (panes, tab) = match self.active() {
            Some((_, t)) => (t.layout.panes().iter().map(|p| p.id.clone()).collect::<Vec<_>>(), t.id.clone()),
            None => (vec![], String::new()),
        };
        if self.zoom.as_ref().is_some_and(|z| *z != tab) {
            self.zoom = None;
        }
        let mut gone: Vec<String> = self.slots.keys().filter(|id| !panes.contains(id)).cloned().collect();
        gone.sort();
        for id in gone {
            self.slots.remove(&id);
            self.actions.push(Action::Detach(id));
        }
        let rects: HashMap<String, Rect> = self.pane_rects().into_iter().collect();
        for id in &panes {
            if !self.slots.contains_key(id) {
                if !self.starting.contains(id) {
                    self.open_slot(id, false, false);
                }
                continue;
            }
            let (Some(slot), Some(r)) = (self.slots.get_mut(id), rects.get(id)) else { continue };
            let size = Self::inner(*r);
            if let Some(view) = &mut slot.view
                && slot.size != size
            {
                slot.size = size;
                view.resize(size.0, size.1);
                self.actions.push(Action::Resize { pane: id.clone(), cols: size.0, rows: size.1 });
            }
        }
        self.restore_hidden();
    }

    /// The terminal started in this run for `pane` that the terminal list
    /// does not show yet.
    fn recently_started(&self, pane: &str) -> Option<String> {
        let addr = self.recent.get(pane)?;
        (self.terminal_of(pane).as_ref() == Some(addr) && self.set.get(addr).is_none()).then(|| addr.clone())
    }

    /// Start, in the background, the missing terminals of the panes that are
    /// not on screen (other tabs), so the whole workspace is running.
    fn restore_hidden(&mut self) {
        let set = &self.set;
        self.recent.retain(|_, a| set.get(a).is_none());
        if !self.auto_restore {
            return;
        }
        let Some(ws) = self.open.clone() else { return };
        let Some((w, active)) = self.active().map(|(w, t)| (w, t.id.clone())) else { return };
        let panes: Vec<String> =
            w.tabs.iter().filter(|t| t.id != active).flat_map(|t| t.layout.panes()).map(|p| p.id.clone()).collect();
        for pane in panes {
            if self.starting.contains(&pane) || self.failed.contains(&pane) || self.recently_started(&pane).is_some() {
                continue;
            }
            if let Some(Plan::Start { pane, params }) = plan(&self.doc, &self.set, &ws, &pane, false) {
                self.starting.insert(pane.clone());
                self.actions.push(Action::Start { pane, params });
            }
        }
    }

    /// A background start finished (the terminal is recorded in the document
    /// before this arrives).
    pub fn on_started(&mut self, pane: &str, result: Result<String, String>) {
        self.starting.remove(pane);
        match result {
            Ok(addr) => {
                self.recent.insert(pane.to_string(), addr);
            }
            Err(e) => {
                self.failed.insert(pane.to_string());
                self.flash_error(&format!("can't restore a terminal: {e}"));
            }
        }
        self.sync();
    }

    /// `manual`: the person asked (Enter), so `restore = false` does not apply.
    fn open_slot(&mut self, pane: &str, fresh: bool, manual: bool) {
        let Some(ws) = self.open.clone() else { return };
        let p = match self.recently_started(pane).filter(|_| !fresh) {
            Some(addr) => Plan::Attach { pane: pane.to_string(), addr },
            None => match plan(&self.doc, &self.set, &ws, pane, fresh) {
                Some(p) => p,
                None => return,
            },
        };
        self.failed.remove(pane);
        let (cols, rows) = self.size_of(pane);
        self.next_attempt += 1;
        let attempt = self.next_attempt;
        let mut slot = Slot { attempt, view: None, error: None, idle: false, size: (cols, rows) };
        match &p {
            Plan::Ended { addr, why, .. } => {
                let mut view = PaneView::new(addr.clone(), addr.clone(), cols, rows);
                view.ended = Some(why.clone());
                slot.view = Some(view);
                self.slots.insert(pane.to_string(), slot);
                return;
            }
            Plan::Start { .. } if !manual && !self.auto_restore => {
                slot.idle = true;
                self.slots.insert(pane.to_string(), slot);
                return;
            }
            Plan::Start { .. } => {
                self.starting.insert(pane.to_string());
            }
            Plan::Attach { .. } => {}
        }
        self.slots.insert(pane.to_string(), slot);
        self.actions.push(Action::Open { plan: p, attempt, cols, rows });
    }

    /// Open the workspace (when it is not open already).
    pub fn open_workspace(&mut self, id: &str) {
        if self.open.as_deref() != Some(id) {
            self.open = Some(id.to_string());
            self.sync();
        }
    }

    /// Enter on an ended pane starts a fresh shell (or its command) there; on a
    /// failed one it tries the same again. A start already underway is left alone.
    fn restart(&mut self, pane: &str, fresh: bool) {
        if self.starting.contains(pane) {
            return;
        }
        self.slots.remove(pane);
        self.actions.push(Action::Detach(pane.to_string()));
        self.open_slot(pane, fresh, true);
    }

    /// An open attempt finished. False when the pane has moved on (another tab
    /// or workspace, or a newer attempt): the caller then closes the stream.
    pub fn on_opened(&mut self, pane: &str, attempt: u64, result: Result<(String, u16, u16), String>) -> bool {
        let was_start = self.starting.remove(pane);
        if was_start && let Ok((addr, ..)) = &result {
            self.recent.insert(pane.to_string(), addr.clone());
        }
        let Some(slot) = self.slots.get_mut(pane).filter(|s| s.attempt == attempt) else { return false };
        let ok = result.is_ok();
        match result {
            Ok((addr, cols, rows)) => {
                slot.view = Some(PaneView::new(addr.clone(), addr, cols, rows));
                slot.size = (cols, rows);
                slot.error = None;
            }
            Err(e) => slot.error = Some(e),
        }
        // The layout may have changed while we were attaching.
        self.sync();
        ok
    }

    /// Output or the end of a pane's stream.
    pub fn on_stream(&mut self, pane: &str, msg: hc_client::StreamMsg) {
        use hc_client::StreamMsg;
        let Some(view) = self.slots.get_mut(pane).and_then(|s| s.view.as_mut()) else { return };
        match msg {
            StreamMsg::Data(bytes) => view.parser.process(&bytes),
            StreamMsg::Resize { .. } => {}
            StreamMsg::Closed(reason) => view.ended = Some(crate::attach::describe_close(reason.as_ref())),
        }
    }

    /// The focused pane takes keys: attached and its program still runs.
    pub fn pane_live(&self) -> bool {
        self.focused_pane()
            .and_then(|id| self.slots.get(&id))
            .and_then(|s| s.view.as_ref())
            .is_some_and(|v| v.ended.is_none())
    }

    /// Cursor-key mode of the focused pane's program, for encoding keys.
    pub fn focused_app_cursor(&self) -> bool {
        self.focused_pane()
            .and_then(|id| self.slots.get(&id))
            .and_then(|s| s.view.as_ref())
            .is_some_and(|v| v.parser.screen().application_cursor())
    }

    pub fn focused_bracketed_paste(&self) -> bool {
        self.focused_pane()
            .and_then(|id| self.slots.get(&id))
            .and_then(|s| s.view.as_ref())
            .is_some_and(|v| v.parser.screen().bracketed_paste())
    }

    /// The terminal address recorded for a pane.
    fn terminal_of(&self, pane: &str) -> Option<String> {
        self.doc.pane(pane).and_then(|(_, _, p)| p.terminal.clone())
    }

    /// Apply an edit to our copy right away (so the next key already sees it)
    /// and send it to the document.
    fn edit(&mut self, op: WsOp) {
        let _ = workspace::apply(&mut self.doc, &op);
        self.actions.push(Action::Update(vec![op]));
        self.sync();
    }

    // ---- messages -------------------------------------------------------------

    pub fn flash_error(&mut self, msg: &str) {
        self.flash = Some(Flash { msg: msg.into(), error: true, at: Instant::now() });
    }

    /// The message to show now, and whether it is an error.
    pub fn current_flash(&self) -> Option<(&str, bool)> {
        self.flash.as_ref().filter(|f| f.at.elapsed() < Duration::from_secs(5)).map(|f| (f.msg.as_str(), f.error))
    }

    // ---- keys -----------------------------------------------------------------

    /// Handle a key; returns what to type into the pane's program, if anything.
    pub fn on_key(&mut self, k: KeyEvent) -> Option<Out> {
        let out = self.key(k);
        self.sync();
        out
    }

    fn key(&mut self, k: KeyEvent) -> Option<Out> {
        if self.dialog.is_some() {
            self.dialog_key(k);
            return None;
        }
        if self.armed {
            self.armed = false;
            return self.prefix_command(k);
        }
        if self.prefix.matches(&k) {
            self.armed = true;
            return None;
        }
        match self.focus {
            Focus::Sidebar => {
                self.sidebar_key(k);
                None
            }
            Focus::Workspace => self.workspace_key(k),
        }
    }

    /// Pasted text: into a dialog field, or back to the caller for the pane.
    pub fn on_paste(&mut self, text: &str) -> Option<String> {
        let to_pane = self.focus == Focus::Workspace && !self.armed && self.pane_live();
        match &mut self.dialog {
            Some(Dialog::Form(f)) => {
                f.fields[f.focus].input.insert_str(text);
                f.error = None;
                None
            }
            Some(_) => None,
            None => to_pane.then(|| text.to_string()),
        }
    }

    fn prefix_command(&mut self, k: KeyEvent) -> Option<Out> {
        if self.prefix.matches(&k) {
            return self.pane_live().then(|| Out::Bytes(vec![self.prefix.byte()]));
        }
        match k.code {
            KeyCode::Char('w') => {
                self.focus = if self.focus == Focus::Sidebar { Focus::Workspace } else { Focus::Sidebar };
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help(0)),
            KeyCode::Char('m') => self.dialog = Some(Dialog::Menu(0)),
            KeyCode::Char('n') => self.add_workspace_form(),
            KeyCode::Char('N') => self.add_project_form(),
            KeyCode::Char('t') => self.new_tab(None),
            KeyCode::Char('T') => self.command_form(),
            KeyCode::Char('v') => self.split(Dir::Horizontal, None),
            KeyCode::Char('s') => self.split(Dir::Vertical, None),
            KeyCode::Char('V') => self.split_form(Dir::Horizontal),
            KeyCode::Char('S') => self.split_form(Dir::Vertical),
            KeyCode::Char('h') | KeyCode::Left => self.move_focus(Nav::Left),
            KeyCode::Char('j') | KeyCode::Down => self.move_focus(Nav::Down),
            KeyCode::Char('k') | KeyCode::Up => self.move_focus(Nav::Up),
            KeyCode::Char('l') | KeyCode::Right => self.move_focus(Nav::Right),
            KeyCode::Char('H') => self.resize_focused(Dir::Horizontal, -RESIZE_STEP),
            KeyCode::Char('L') => self.resize_focused(Dir::Horizontal, RESIZE_STEP),
            KeyCode::Char('K') => self.resize_focused(Dir::Vertical, -RESIZE_STEP),
            KeyCode::Char('J') => self.resize_focused(Dir::Vertical, RESIZE_STEP),
            KeyCode::Char(c @ '1'..='9') => self.go_tab(Some(c as usize - '1' as usize), 0),
            KeyCode::Char('[') => self.go_tab(None, -1),
            KeyCode::Char(']') => self.go_tab(None, 1),
            KeyCode::Char('r') => self.rename_tab_form(),
            KeyCode::Char('x') => self.close_pane_confirm(),
            KeyCode::Char('X') => self.close_tab_confirm(),
            KeyCode::Char('z') => self.toggle_zoom(),
            _ => {} // Esc and anything unknown cancel
        }
        None
    }

    fn workspace_key(&mut self, k: KeyEvent) -> Option<Out> {
        let id = self.focused_pane()?;
        let slot = self.slots.get(&id)?;
        let ended = slot.view.as_ref().is_some_and(|v| v.ended.is_some());
        if (ended || slot.error.is_some() || slot.idle) && k.code == KeyCode::Enter {
            // A fresh shell where the program ended; a plain retry after a
            // failed attach, or the first start of a pane left alone.
            self.restart(&id, ended);
            return None;
        }
        self.pane_live().then_some(Out::Key(k))
    }

    // ---- tabs and splits ----------------------------------------------------

    /// The open workspace, or a hint when there is none.
    fn need_workspace(&mut self) -> Option<String> {
        let id = self.active().map(|(w, _)| w.id.clone());
        if id.is_none() {
            self.flash_error("open a workspace first");
        }
        id
    }

    /// A tab name that is free in the workspace: `base`, `base 2`, `base 3`…
    fn free_tab_name(w: &Workspace, base: &str) -> String {
        let taken = |n: &str| w.tabs.iter().any(|t| t.name == n);
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).unwrap()
    }

    /// The tab's name for a command: its program (`nvim .` is `nvim`).
    fn tab_base(command: Option<&str>) -> String {
        let prog = command.and_then(|c| c.split_whitespace().next()).and_then(|p| p.rsplit('/').next());
        prog.filter(|p| !p.is_empty()).unwrap_or("shell").to_string()
    }

    /// The ops for a new tab running `command` (a shell when empty).
    fn new_tab_ops(&self, workspace: &str, command: Option<String>) -> Option<Vec<WsOp>> {
        let (_, w) = self.doc.workspace(workspace)?;
        let pane_id = new_id("n");
        let mut ops = vec![WsOp::AddTab {
            workspace: workspace.to_string(),
            id: new_id("t"),
            name: Self::free_tab_name(w, &Self::tab_base(command.as_deref())),
            pane_id: pane_id.clone(),
        }];
        if command.is_some() {
            ops.push(WsOp::SetCommand { pane: pane_id, command });
        }
        Some(ops)
    }

    fn new_tab(&mut self, command: Option<String>) {
        let Some(ws) = self.need_workspace() else { return };
        if let Some(ops) = self.new_tab_ops(&ws, command) {
            self.actions.push(Action::Update(ops));
        }
    }

    fn split_ops(&self, pane: &str, dir: Dir, command: Option<String>) -> Vec<WsOp> {
        let new_id = new_id("n");
        let mut ops = vec![WsOp::SplitPane { pane: pane.to_string(), dir, new_id: new_id.clone() }];
        if command.is_some() {
            ops.push(WsOp::SetCommand { pane: new_id, command });
        }
        ops
    }

    fn split(&mut self, dir: Dir, command: Option<String>) {
        if self.need_workspace().is_none() {
            return;
        }
        let Some(pane) = self.focused_pane() else { return };
        self.zoom = None;
        let ops = self.split_ops(&pane, dir, command);
        self.actions.push(Action::Update(ops));
    }

    fn command_form(&mut self) {
        let Some(workspace) = self.need_workspace() else { return };
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::NewTab { workspace },
            title: "new tab: command (empty = shell)".into(),
            fields: vec![Self::field("command", "")],
            focus: 0,
            error: None,
        }));
    }

    fn split_form(&mut self, dir: Dir) {
        if self.need_workspace().is_none() {
            return;
        }
        let Some(pane) = self.focused_pane() else { return };
        let side = if dir == Dir::Horizontal { "side by side" } else { "stacked" };
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::NewSplit { pane, dir },
            title: format!("split {side}: command (empty = shell)"),
            fields: vec![Self::field("command", "")],
            focus: 0,
            error: None,
        }));
    }

    fn move_focus(&mut self, nav: Nav) {
        let Some((_, tab)) = self.active() else { return };
        let Some(from) = self.focused_pane() else { return };
        // Move by the real layout, so leave a zoom first.
        let rects = workspace::rects(&tab.layout, self.panes_area);
        self.zoom = None;
        if let Some(pane) = neighbor(&rects, &from, nav) {
            self.edit(WsOp::SetFocus { pane });
        }
    }

    fn resize_focused(&mut self, dir: Dir, delta: i16) {
        let Some((_, tab)) = self.active() else { return };
        let Some(pane) = self.focused_pane() else { return };
        let tab = tab.id.clone();
        self.zoom = None;
        self.edit(WsOp::ResizePane { tab, pane, dir, delta });
    }

    /// Switch to tab `index`, or step from the current one by `step` (wrapping).
    fn go_tab(&mut self, index: Option<usize>, step: i32) {
        let Some((w, _)) = self.active() else { return };
        let n = w.tabs.len();
        let to = match index {
            Some(i) if i < n => i,
            Some(i) => return self.flash_error(&format!("no tab {}", i + 1)),
            None => (w.active_tab as i32 + step).rem_euclid(n as i32) as usize,
        };
        if to != w.active_tab {
            let workspace = w.id.clone();
            self.edit(WsOp::SetActiveTab { workspace, index: to });
        }
    }

    fn toggle_zoom(&mut self) {
        let Some((_, t)) = self.active() else { return };
        let id = t.id.clone();
        self.zoom = if self.zoom.as_deref() == Some(id.as_str()) { None } else { Some(id) };
    }

    fn rename_tab_form(&mut self) {
        let Some((_, t)) = self.active() else {
            self.flash_error("open a workspace first");
            return;
        };
        let (id, name) = (t.id.clone(), t.name.clone());
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::RenameTab { id },
            title: "rename tab".into(),
            fields: vec![Self::field("name", name)],
            focus: 0,
            error: None,
        }));
    }

    /// Check an op against a copy of the document, so a refusal is shown at once.
    fn check(&self, ops: &[WsOp]) -> Result<(), String> {
        let mut scratch = self.doc.clone();
        ops.iter().try_for_each(|op| workspace::apply(&mut scratch, op))
    }

    fn close_pane_confirm(&mut self) {
        if self.need_workspace().is_none() {
            return;
        }
        let Some(pane) = self.focused_pane() else { return };
        if let Err(e) = self.check(&[WsOp::ClosePane { pane: pane.clone() }]) {
            return self.flash_error(&e);
        }
        let what = self.doc.pane(&pane).and_then(|(_, _, p)| p.command.clone()).unwrap_or_else(|| "the shell".into());
        let message = format!("Close this pane? Its program ({what}) will end.  y / n");
        self.dialog = Some(Dialog::Confirm { kind: ConfirmKind::ClosePane(pane), message });
    }

    fn close_tab_confirm(&mut self) {
        let Some((_, t)) = self.active() else {
            self.flash_error("open a workspace first");
            return;
        };
        let (id, name, n) = (t.id.clone(), t.name.clone(), t.layout.panes().len());
        if let Err(e) = self.check(&[WsOp::CloseTab { id: id.clone() }]) {
            return self.flash_error(&e);
        }
        let message = format!("Close tab \"{name}\" and its {n} pane(s)? Their programs will end.  y / n");
        self.dialog = Some(Dialog::Confirm { kind: ConfirmKind::CloseTab(id), message });
    }

    fn sidebar_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_sel(-(self.rows().len() as i32)),
            KeyCode::Char('G') | KeyCode::End => self.move_sel(self.rows().len() as i32),
            KeyCode::Enter => match self.selected() {
                Some(Row::Workspace(id)) => {
                    self.open_workspace(&id);
                    self.focus = Focus::Workspace;
                }
                Some(Row::Project(id)) => self.toggle_collapse(&id),
                None => {}
            },
            KeyCode::Char('z') => {
                if let Some(p) = self.current_project() {
                    self.toggle_collapse(&p);
                }
            }
            KeyCode::Char('a') => self.add_workspace_form(),
            KeyCode::Char('A') => self.add_project_form(),
            KeyCode::Char('r') => self.rename_form(),
            KeyCode::Char('D') => self.delete_confirm(),
            KeyCode::Char('J') => self.reorder(Move::Down),
            KeyCode::Char('K') => self.reorder(Move::Up),
            KeyCode::Esc | KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Workspace,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help(0)),
            _ => {}
        }
    }

    fn toggle_collapse(&mut self, project: &str) {
        let Some(p) = self.doc.project(project) else { return };
        let collapsed = !p.collapsed;
        if collapsed && self.doc.workspace(&self.sel).is_some_and(|(q, _)| q.id == project) {
            self.sel = project.to_string();
        }
        self.actions.push(Action::Update(vec![WsOp::SetCollapsed { id: project.to_string(), collapsed }]));
    }

    fn reorder(&mut self, dir: Move) {
        let op = match self.selected() {
            Some(Row::Project(id)) => WsOp::MoveProject { id, dir },
            Some(Row::Workspace(id)) => WsOp::MoveWorkspace { id, dir },
            None => return,
        };
        self.actions.push(Action::Update(vec![op]));
    }

    // ---- dialogs ----------------------------------------------------------------

    fn field(label: &'static str, text: impl Into<String>) -> Field {
        Field { label, input: TextInput::new(text) }
    }

    pub fn add_project_form(&mut self) {
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::AddProject,
            title: "new project".into(),
            fields: vec![Self::field("name", ""), Self::field("folder", self.launch_dir.clone())],
            focus: 0,
            error: None,
        }));
    }

    pub fn add_workspace_form(&mut self) {
        let Some(pid) = self.current_project() else {
            self.flash_error("add a project first");
            return;
        };
        let (name, root) = self.doc.project(&pid).map(|p| (p.name.clone(), p.root.clone())).unwrap_or_default();
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::AddWorkspace { project: pid },
            title: format!("new workspace in {name}"),
            fields: vec![Self::field("name", ""), Self::field("folder", root)],
            focus: 0,
            error: None,
        }));
    }

    fn rename_form(&mut self) {
        let (id, what, name) = match self.selected() {
            Some(Row::Project(id)) => {
                let name = self.doc.project(&id).map(|p| p.name.clone()).unwrap_or_default();
                (id, "project", name)
            }
            Some(Row::Workspace(id)) => {
                let name = self.doc.workspace(&id).map(|(_, w)| w.name.clone()).unwrap_or_default();
                (id, "workspace", name)
            }
            None => return,
        };
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::Rename { id },
            title: format!("rename {what}"),
            fields: vec![Self::field("name", name)],
            focus: 0,
            error: None,
        }));
    }

    /// The terminals of every pane of a workspace or project.
    fn terminals_of(&self, id: &str) -> Vec<String> {
        let ws: Vec<&Workspace> = match self.doc.project(id) {
            Some(p) => p.workspaces.iter().collect(),
            None => self.doc.workspace(id).map(|(_, w)| w).into_iter().collect(),
        };
        ws.iter().flat_map(|w| w.panes()).filter_map(|p| p.terminal.clone()).collect()
    }

    fn delete_confirm(&mut self) {
        let what = match self.selected() {
            Some(Row::Project(id)) => {
                let Some(p) = self.doc.project(&id) else { return };
                format!("project \"{}\" and its {} workspace(s)", p.name, p.workspaces.len())
            }
            Some(Row::Workspace(id)) => {
                let Some((_, w)) = self.doc.workspace(&id) else { return };
                format!("workspace \"{}\"", w.name)
            }
            None => return,
        };
        let message = format!("Delete {what}?  y: end its terminals and delete · k: keep them running and delete · n: cancel");
        self.dialog = Some(Dialog::Confirm { kind: ConfirmKind::Delete(self.sel.clone()), message });
    }

    fn dialog_key(&mut self, k: KeyEvent) {
        match self.dialog.as_mut() {
            Some(Dialog::Help(top)) => {
                // The help may be taller than the screen: it scrolls, any other key closes.
                let max = ws_ui::help_lines("").len().saturating_sub(self.screen_h.saturating_sub(4) as usize);
                match k.code {
                    KeyCode::Char('j') | KeyCode::Down => *top = (*top + 1).min(max),
                    KeyCode::Char('k') | KeyCode::Up => *top = top.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => *top = (*top + 10).min(max),
                    KeyCode::PageUp => *top = top.saturating_sub(10),
                    KeyCode::Char('g') | KeyCode::Home => *top = 0,
                    KeyCode::Char('G') | KeyCode::End => *top = max,
                    _ => self.dialog = None,
                }
            }
            Some(Dialog::Confirm { kind, .. }) => match k.code {
                KeyCode::Char('y' | 'Y') => {
                    let kind = kind.clone();
                    self.dialog = None;
                    self.confirmed(kind);
                }
                // Only a delete has a third answer: keep the terminals.
                KeyCode::Char('k' | 'K') if matches!(kind, ConfirmKind::Delete(_)) => {
                    let ConfirmKind::Delete(id) = kind.clone() else { return };
                    self.dialog = None;
                    self.delete(id, false);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.dialog = None,
                _ => {}
            },
            Some(Dialog::Menu(i)) => match k.code {
                KeyCode::Char('j') | KeyCode::Down => *i = (*i + 1).min(MENU.len() - 1),
                KeyCode::Char('k') | KeyCode::Up => *i = i.saturating_sub(1),
                KeyCode::Enter => {
                    let start = MENU[*i].1;
                    self.dialog = None;
                    self.actions.push(Action::Board(start));
                }
                KeyCode::Esc | KeyCode::Char('q') => self.dialog = None,
                _ => {}
            },
            Some(Dialog::Form(f)) => match k.code {
                KeyCode::Esc => self.dialog = None,
                KeyCode::Tab | KeyCode::Down => f.focus = (f.focus + 1) % f.fields.len(),
                KeyCode::BackTab | KeyCode::Up => f.focus = (f.focus + f.fields.len() - 1) % f.fields.len(),
                KeyCode::Enter if f.focus + 1 < f.fields.len() => f.focus += 1,
                KeyCode::Enter => self.submit_form(),
                _ => {
                    if f.fields[f.focus].input.handle(k) {
                        f.error = None;
                    }
                }
            },
            None => {}
        }
    }

    /// The answer was yes.
    fn confirmed(&mut self, kind: ConfirmKind) {
        match kind {
            ConfirmKind::Delete(id) => self.delete(id, true),
            ConfirmKind::ClosePane(pane) => {
                let terminals: Vec<String> = self.terminal_of(&pane).into_iter().collect();
                self.close_with(terminals, WsOp::ClosePane { pane });
            }
            ConfirmKind::CloseTab(id) => {
                let terminals = match self.doc.tab(&id) {
                    Some((_, t)) => t.layout.panes().iter().filter_map(|p| p.terminal.clone()).collect(),
                    None => vec![],
                };
                self.close_with(terminals, WsOp::CloseTab { id });
            }
        }
    }

    /// Delete a project or workspace; with `kill` its terminals end first.
    fn delete(&mut self, id: String, kill: bool) {
        let terminals = if kill { self.terminals_of(&id) } else { vec![] };
        let op = if self.doc.project(&id).is_some() { WsOp::DeleteProject { id } } else { WsOp::DeleteWorkspace { id } };
        if !terminals.is_empty() {
            self.actions.push(Action::Kill(terminals));
        }
        self.actions.push(Action::Update(vec![op]));
    }

    /// End the programs, then drop the pane or tab from the document.
    fn close_with(&mut self, terminals: Vec<String>, op: WsOp) {
        if let Err(e) = self.check(std::slice::from_ref(&op)) {
            return self.flash_error(&e);
        }
        self.zoom = None;
        if !terminals.is_empty() {
            self.actions.push(Action::Kill(terminals));
        }
        self.actions.push(Action::Update(vec![op]));
    }

    /// Check the form against the document first, so mistakes show up inside
    /// the dialog instead of after it closed.
    fn submit_form(&mut self) {
        let Some(Dialog::Form(form)) = self.dialog.take() else { return };
        let built = self.build_op(&form);
        match built {
            Ok((ops, pending_sel, pending_open)) => {
                if let Err(e) = self.check(&ops) {
                    return self.form_error(form, e);
                }
                self.pending_sel = pending_sel;
                self.pending_open = pending_open;
                self.zoom = None;
                self.actions.push(Action::Update(ops));
            }
            Err(e) => self.form_error(form, e),
        }
    }

    fn form_error(&mut self, mut form: Form, e: String) {
        form.error = Some(e);
        self.dialog = Some(Dialog::Form(form));
    }

    /// The op, and the project / workspace to select or open once it lands.
    #[allow(clippy::type_complexity)]
    fn build_op(&self, form: &Form) -> Result<(Vec<WsOp>, Option<String>, Option<String>), String> {
        let name = form.fields[0].input.value().trim().to_string();
        let folder = || existing_dir(form.fields[1].input.value());
        Ok(match &form.kind {
            FormKind::AddProject => {
                let id = new_id("p");
                (vec![WsOp::AddProject { id: id.clone(), name, root: folder()? }], Some(id), None)
            }
            FormKind::AddWorkspace { project } => {
                let id = new_id("w");
                let op = WsOp::AddWorkspace {
                    project: project.clone(),
                    id: id.clone(),
                    name,
                    folder: folder()?,
                    tab_id: new_id("t"),
                    pane_id: new_id("n"),
                };
                (vec![op], None, Some(id))
            }
            FormKind::Rename { id } => {
                let op = if self.doc.project(id).is_some() {
                    WsOp::RenameProject { id: id.clone(), name }
                } else {
                    WsOp::RenameWorkspace { id: id.clone(), name }
                };
                (vec![op], None, None)
            }
            FormKind::RenameTab { id } => (vec![WsOp::RenameTab { id: id.clone(), name }], None, None),
            FormKind::NewTab { workspace } => {
                let ops = self.new_tab_ops(workspace, command(&form.fields[0].input));
                (ops.ok_or("the workspace is gone")?, None, None)
            }
            FormKind::NewSplit { pane, dir } => (self.split_ops(pane, *dir, command(&form.fields[0].input)), None, None),
        })
    }
}

/// What was typed into a command prompt; empty means the shell.
fn command(input: &TextInput) -> Option<String> {
    Some(input.value().trim().to_string()).filter(|c| !c.is_empty())
}

/// An existing folder as an absolute path (`~` and relative paths allowed).
fn existing_dir(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the folder can't be empty".into());
    }
    let abs = expand_home(text).canonicalize().map_err(|_| format!("{text}: no such folder"))?;
    if !abs.is_dir() {
        return Err(format!("{text} is not a folder"));
    }
    Ok(abs.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::apply;
    use hc_client::Terminal;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ctrl_space() -> KeyEvent {
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
    }

    fn add_project(doc: &mut WsDoc, id: &str, name: &str) {
        apply(doc, &WsOp::AddProject { id: id.into(), name: name.into(), root: "/tmp".into() }).unwrap();
    }

    fn add_ws(doc: &mut WsDoc, project: &str, id: &str, name: &str) {
        let op = WsOp::AddWorkspace {
            project: project.into(),
            id: id.into(),
            name: name.into(),
            folder: "/tmp".into(),
            tab_id: format!("t-{id}"),
            pane_id: format!("n-{id}"),
        };
        apply(doc, &op).unwrap();
    }

    /// shosha: frontend, backend; solo: main.
    fn app() -> WsApp {
        let mut doc = WsDoc::default();
        add_project(&mut doc, "p1", "shosha");
        add_ws(&mut doc, "p1", "w1", "frontend");
        add_ws(&mut doc, "p1", "w2", "backend");
        add_project(&mut doc, "p2", "solo");
        add_ws(&mut doc, "p2", "w3", "main");
        let mut a = WsApp::new(Prefix::Space, None);
        // As if the first snapshot of the terminals had arrived.
        a.synced = true;
        a.on_doc(doc);
        a.actions.clear();
        a
    }

    fn terminal(addr: &str, live: bool) -> Terminal {
        serde_json::from_value(serde_json::json!({
            "address": addr,
            "state": if live { "live" } else { "finished" },
        }))
        .unwrap()
    }

    fn finished(addr: &str, reason: &str, code: Option<i32>) -> Terminal {
        serde_json::from_value(serde_json::json!({
            "address": addr,
            "state": "finished",
            "reason": reason,
            "exit_code": code,
        }))
        .unwrap()
    }

    /// Open `ws` and answer every attach/start as if it worked: each pane gets
    /// terminal `t-<pane>`. The actions are cleared afterwards.
    fn open_attached(a: &mut WsApp, ws: &str) {
        a.open_workspace(ws);
        attach_pending(a);
    }

    fn attach_pending(a: &mut WsApp) {
        for act in std::mem::take(&mut a.actions) {
            if let Action::Open { plan, attempt, cols, rows } = act {
                let pane = plan.pane().to_string();
                a.on_opened(&pane, attempt, Ok((format!("t-{pane}"), cols, rows)));
            }
        }
        // Resizes caused by the attaches themselves are not interesting.
        a.actions.clear();
    }

    fn opens(a: &mut WsApp) -> Vec<Plan> {
        std::mem::take(&mut a.actions)
            .into_iter()
            .filter_map(|x| match x {
                Action::Open { plan, .. } => Some(plan),
                _ => None,
            })
            .collect()
    }

    fn ids(a: &WsApp) -> Vec<String> {
        a.rows().iter().map(|r| r.id().to_string()).collect()
    }

    fn only_update(a: &mut WsApp) -> Vec<WsOp> {
        let mut acts = std::mem::take(&mut a.actions);
        assert_eq!(acts.len(), 1, "{acts:?}");
        match acts.remove(0) {
            Action::Update(ops) => ops,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn first_load_lands_on_the_sidebar_at_the_first_row() {
        let a = app();
        assert_eq!(a.focus, Focus::Sidebar);
        assert_eq!(a.sel, "p1");
        assert_eq!(ids(&a), ["p1", "w1", "w2", "p2", "w3"]);
    }

    #[test]
    fn navigation_skips_collapsed_workspaces() {
        let mut a = app();
        a.on_key(key('j'));
        assert_eq!(a.sel, "w1");
        // Collapse shosha from inside: the cursor jumps to the project.
        a.on_key(key('z'));
        assert_eq!(a.sel, "p1");
        let ops = only_update(&mut a);
        assert_eq!(ops, [WsOp::SetCollapsed { id: "p1".into(), collapsed: true }]);
        // The doc comes back collapsed.
        let mut doc = a.doc.clone();
        apply(&mut doc, &ops[0]).unwrap();
        a.on_doc(doc);
        assert_eq!(ids(&a), ["p1", "p2", "w3"]);
        a.on_key(key('j'));
        assert_eq!(a.sel, "p2");
        a.on_key(key('G'));
        assert_eq!(a.sel, "w3");
        a.on_key(key('j'));
        assert_eq!(a.sel, "w3", "stays at the end");
        a.on_key(key('g'));
        assert_eq!(a.sel, "p1");
        // Enter on a project toggles it.
        a.on_key(code(KeyCode::Enter));
        assert_eq!(only_update(&mut a), [WsOp::SetCollapsed { id: "p1".into(), collapsed: false }]);
    }

    #[test]
    fn cursor_moves_off_a_row_that_disappears() {
        let mut a = app();
        a.sel = "w2".into();
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::DeleteWorkspace { id: "w2".into() }).unwrap();
        a.on_doc(doc);
        assert_eq!(a.sel, "p1");
    }

    #[test]
    fn prefix_then_key() {
        let mut a = app();
        a.focus = Focus::Workspace;
        assert_eq!(a.on_key(ctrl_space()), None);
        assert!(a.armed);
        assert_eq!(a.on_key(key('w')), None);
        assert!(!a.armed);
        assert_eq!(a.focus, Focus::Sidebar);

        // NUL arrives as Null in some terminals.
        a.on_key(code(KeyCode::Null));
        assert!(a.armed);
        a.on_key(key('w'));
        assert_eq!(a.focus, Focus::Workspace, "w toggles");

        a.on_key(ctrl_space());
        a.on_key(key('?'));
        assert!(matches!(a.dialog, Some(Dialog::Help(_))));
        a.on_key(key('x'));
        assert!(a.dialog.is_none());

        a.on_key(ctrl_space());
        a.on_key(key('q'));
        assert!(a.quit);
    }

    #[test]
    fn double_prefix_passes_through_and_esc_cancels() {
        let mut a = app();
        a.focus = Focus::Workspace;
        // Nothing to type into yet.
        a.on_key(ctrl_space());
        assert_eq!(a.on_key(ctrl_space()), None);
        open_attached(&mut a, "w1");
        a.on_key(ctrl_space());
        assert_eq!(a.on_key(ctrl_space()), Some(Out::Bytes(vec![0])));
        assert!(!a.armed);

        a.on_key(ctrl_space());
        assert_eq!(a.on_key(code(KeyCode::Esc)), None);
        assert!(!a.armed && !a.quit && a.dialog.is_none());
        // After Esc, `q` is a plain key for the program again.
        assert_eq!(a.on_key(key('q')), Some(Out::Key(key('q'))));
        assert!(!a.quit);

        // A different prefix.
        let mut b = WsApp::new(Prefix::Ctrl('b'), None);
        b.synced = true;
        b.on_doc(a.doc.clone());
        open_attached(&mut b, "w1");
        b.focus = Focus::Workspace;
        assert_eq!(b.on_key(ctrl_space()), Some(Out::Key(ctrl_space())), "ctrl+space is no prefix now");
        b.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(b.armed);
        assert_eq!(b.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)), Some(Out::Bytes(vec![2])));
    }

    #[test]
    fn workspace_focus_passes_keys_only_to_a_live_pane() {
        let mut a = app();
        a.focus = Focus::Workspace;
        assert_eq!(a.on_key(key('x')), None, "nothing open");
        open_attached(&mut a, "w1");
        assert_eq!(a.on_key(key('x')), Some(Out::Key(key('x'))));
        // The sidebar's keys are not special in the workspace.
        assert_eq!(a.on_key(key('q')), Some(Out::Key(key('q'))));
        assert!(!a.quit);
        a.slots.get_mut("n-w1").unwrap().view.as_mut().unwrap().ended = Some("exited".into());
        assert_eq!(a.on_key(key('x')), None);
    }

    #[test]
    fn focus_switching() {
        let mut a = app();
        a.on_key(code(KeyCode::Tab));
        assert_eq!(a.focus, Focus::Workspace);
        a.on_key(ctrl_space());
        a.on_key(key('w'));
        assert_eq!(a.focus, Focus::Sidebar);
        a.on_key(key('l'));
        assert_eq!(a.focus, Focus::Workspace);
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Esc));
        assert_eq!(a.focus, Focus::Workspace);
    }

    #[test]
    fn plan_attaches_to_a_live_terminal_and_starts_otherwise() {
        let mut a = app();
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetTerminal { pane: "n-w1".into(), terminal: Some("t9".into()) }).unwrap();
        apply(&mut doc, &WsOp::SetCommand { pane: "n-w2".into(), command: Some("nvim .".into()) }).unwrap();
        a.doc = doc;

        // Recorded but not in the set: start.
        let Some(Plan::Start { pane, params }) = plan(&a.doc, &a.set, "w1", "n-w1", false) else { panic!() };
        assert_eq!(pane, "n-w1");
        assert_eq!(params.cwd.as_deref(), Some("/tmp"));
        assert_eq!(params.line, None);
        assert_eq!(params.labels[LABEL_WS], "w1");
        assert_eq!(params.labels[LABEL_PANE], "n-w1");
        assert_eq!(params.labels[LABEL_PROJECT], "shosha");

        // Finished on its own: shown as ended, not started again...
        a.set.terminals.insert("t9".into(), finished("t9", "exited", Some(0)));
        assert!(matches!(plan(&a.doc, &a.set, "w1", "n-w1", false), Some(Plan::Ended { why, .. }) if why == "exited (code 0)"));
        // ...unless a fresh one is asked for (Enter).
        assert!(matches!(plan(&a.doc, &a.set, "w1", "n-w1", true), Some(Plan::Start { .. })));
        // Finished because the system went down: start again.
        a.set.terminals.insert("t9".into(), finished("t9", "server_stop", None));
        assert!(matches!(plan(&a.doc, &a.set, "w1", "n-w1", false), Some(Plan::Start { .. })));

        // Live: attach, unless a fresh one is asked for.
        a.set.terminals.insert("t9".into(), terminal("t9", true));
        assert!(matches!(plan(&a.doc, &a.set, "w1", "n-w1", false), Some(Plan::Attach { addr, .. }) if addr == "t9"));
        assert!(matches!(plan(&a.doc, &a.set, "w1", "n-w1", true), Some(Plan::Start { .. })));

        // A pane with a command starts it through the shell line.
        let Some(Plan::Start { params, .. }) = plan(&a.doc, &a.set, "w2", "n-w2", false) else { panic!() };
        assert_eq!(params.line.as_deref(), Some("nvim ."));
        assert!(plan(&a.doc, &a.set, "nope", "n-w1", false).is_none());
    }

    #[test]
    fn opening_switches_and_ignores_repeats() {
        let mut a = app();
        a.sel = "w1".into();
        a.on_key(code(KeyCode::Enter));
        assert_eq!(a.open.as_deref(), Some("w1"));
        assert_eq!(a.focus, Focus::Workspace);
        assert!(matches!(a.actions[..], [Action::Open { plan: Plan::Start { .. }, .. }]));
        a.actions.clear();

        // Still opening: Enter again must not start a second terminal.
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert!(a.actions.is_empty());

        let attempt = a.slots["n-w1"].attempt;
        assert!(a.on_opened("n-w1", attempt, Ok(("t1".into(), 98, 26))));
        a.actions.clear();
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert!(a.actions.is_empty());

        // Another workspace closes the old stream first.
        a.sel = "w2".into();
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert_eq!(a.open.as_deref(), Some("w2"));
        assert!(matches!(&a.actions[..], [Action::Detach(p), Action::Open { .. }] if p == "n-w1"));
        assert!(a.slots.contains_key("n-w2") && !a.slots.contains_key("n-w1"));
        // A late attach for w1 is refused (so the caller closes it).
        assert!(!a.on_opened("n-w1", attempt, Ok(("t1".into(), 98, 26))));
    }

    #[test]
    fn a_late_answer_for_an_older_attempt_is_refused() {
        let mut a = app();
        a.open_workspace("w1");
        let first = a.slots["n-w1"].attempt;
        a.actions.clear();
        a.open_workspace("w2");
        a.open_workspace("w1");
        // The first start is still underway, so no second terminal is started.
        assert!(!a.slots.contains_key("n-w1"));
        assert_eq!(opens(&mut a).len(), 1, "only w2's pane");
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetTerminal { pane: "n-w1".into(), terminal: Some("t1".into()) }).unwrap();
        a.set.terminals.insert("t1".into(), terminal("t1", true));
        assert!(!a.on_opened("n-w1", first, Ok(("t1".into(), 10, 5))), "stale: the stream is closed");
        // Now that the start finished, the pane opens again, attaching this time.
        a.on_doc(doc);
        let again = opens(&mut a);
        assert!(matches!(&again[..], [Plan::Attach { addr, .. }] if addr == "t1"), "{again:?}");
    }

    #[test]
    fn enter_restarts_an_ended_pane() {
        let mut a = app();
        open_attached(&mut a, "w1");
        a.focus = Focus::Workspace;
        a.slots.get_mut("n-w1").unwrap().view.as_mut().unwrap().ended = Some("exited (code 0)".into());
        a.on_key(code(KeyCode::Enter));
        let [Action::Detach(old), Action::Open { plan: Plan::Start { pane, .. }, .. }] = &a.actions[..] else {
            panic!("{:?}", a.actions)
        };
        assert_eq!((old.as_str(), pane.as_str()), ("n-w1", "n-w1"));
        // A second Enter while it starts does nothing.
        a.actions.clear();
        a.on_key(code(KeyCode::Enter));
        assert!(a.actions.is_empty());
    }

    #[test]
    fn deleting_the_open_workspace_closes_its_panes() {
        let mut a = app();
        open_attached(&mut a, "w1");
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::DeleteWorkspace { id: "w1".into() }).unwrap();
        a.on_doc(doc);
        assert!(a.open.is_none() && a.slots.is_empty());
        assert!(matches!(&a.actions[..], [Action::Detach(p)] if p == "n-w1"));
    }

    #[test]
    fn restore_reopens_the_remembered_workspace_once() {
        let mut a = WsApp::new(Prefix::Space, Some("w2".into()));
        let mut doc = WsDoc::default();
        add_project(&mut doc, "p1", "shosha");
        add_ws(&mut doc, "p1", "w2", "backend");
        a.on_doc(doc.clone());
        assert_eq!((a.open.as_deref(), a.focus), (Some("w2"), Focus::Workspace));
        let mut b = WsApp::new(Prefix::Space, Some("gone".into()));
        b.on_doc(doc);
        assert_eq!((b.open.as_deref(), b.focus), (None, Focus::Sidebar));
    }

    #[test]
    fn reorder_rename_delete_make_the_right_ops() {
        let mut a = app();
        a.sel = "w2".into();
        a.on_key(key('K'));
        assert_eq!(only_update(&mut a), [WsOp::MoveWorkspace { id: "w2".into(), dir: Move::Up }]);
        a.sel = "p1".into();
        a.on_key(key('J'));
        assert_eq!(only_update(&mut a), [WsOp::MoveProject { id: "p1".into(), dir: Move::Down }]);

        // Rename a workspace.
        a.sel = "w1".into();
        a.on_key(key('r'));
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert_eq!(f.fields[0].input.value(), "frontend");
        for _ in 0..8 {
            a.on_key(code(KeyCode::Backspace));
        }
        for c in "web".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert_eq!(only_update(&mut a), [WsOp::RenameWorkspace { id: "w1".into(), name: "web".into() }]);

        // A clash shows inside the dialog and nothing is sent.
        a.sel = "p1".into();
        a.on_key(key('r'));
        for _ in 0..6 {
            a.on_key(code(KeyCode::Backspace));
        }
        for c in "solo".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        let Some(Dialog::Form(f)) = &a.dialog else { panic!("dialog closed") };
        assert!(f.error.as_deref().unwrap().contains("already exists"));
        assert!(a.actions.is_empty());
        a.on_key(code(KeyCode::Esc));

        // Delete asks first and says terminals survive.
        a.sel = "w3".into();
        a.on_key(key('D'));
        let Some(Dialog::Confirm { message, .. }) = &a.dialog else { panic!() };
        assert!(message.contains("keep them running"));
        a.on_key(key('x'));
        assert!(a.dialog.is_some(), "other keys don't answer");
        a.on_key(key('n'));
        assert!(a.dialog.is_none() && a.actions.is_empty());
        a.on_key(key('D'));
        a.on_key(key('y'));
        assert_eq!(only_update(&mut a), [WsOp::DeleteWorkspace { id: "w3".into() }]);
        a.sel = "p2".into();
        a.on_key(key('D'));
        a.on_key(key('y'));
        assert_eq!(only_update(&mut a), [WsOp::DeleteProject { id: "p2".into() }]);
    }

    #[test]
    fn add_forms_validate_the_folder_and_open_the_new_workspace() {
        let mut a = app();
        a.sel = "w2".into();
        a.on_key(key('a'));
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert_eq!(f.fields[1].input.value(), "/tmp", "folder defaults to the project root");
        for c in "api".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Tab));
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "/definitely/not/here".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        let Some(Dialog::Form(f)) = &a.dialog else { panic!("closed") };
        assert!(f.error.as_deref().unwrap().contains("no such folder"));
        assert!(a.actions.is_empty());

        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "/tmp".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        let ops = only_update(&mut a);
        let [WsOp::AddWorkspace { project, id, name, folder, .. }] = &ops[..] else { panic!("{ops:?}") };
        assert_eq!((project.as_str(), name.as_str()), ("p1", "api"));
        assert!(folder.starts_with('/'));

        // When the doc comes back the new workspace opens with focus.
        let mut doc = a.doc.clone();
        apply(&mut doc, &ops[0]).unwrap();
        a.on_doc(doc);
        assert_eq!(a.open.as_deref(), Some(id.as_str()));
        assert_eq!((a.sel.as_str(), a.focus), (id.as_str(), Focus::Workspace));
    }

    #[test]
    fn add_workspace_needs_a_project() {
        let mut a = WsApp::new(Prefix::Space, None);
        a.on_key(key('a'));
        assert!(a.dialog.is_none());
        assert!(a.current_flash().is_some_and(|(m, e)| e && m.contains("project")));
        a.on_key(key('A'));
        assert!(matches!(a.dialog, Some(Dialog::Form(_))));
    }

    #[test]
    fn status_alerts_fire_on_change_only() {
        let mut a = app();
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetTerminal { pane: "n-w2".into(), terminal: Some("t5".into()) }).unwrap();
        a.on_doc(doc);
        let mut t = terminal("t5", true);
        a.set.terminals.insert("t5".into(), t.clone());
        a.synced = true;
        assert!(a.status_alerts().is_empty());
        t.status = Some(hc_client::Status {
            state: "blocked".into(),
            app: String::new(),
            kind: Some("permission".into()),
            msg: String::new(),
            progress: None,
            title: String::new(),
        });
        a.set.terminals.insert("t5".into(), t);
        assert_eq!(a.ws_status("w2"), WsStatus::Attention);
        assert_eq!(a.project_status("p1"), WsStatus::Attention);
        assert_eq!(a.attention(), Some(("shosha".into(), "backend".into())));
        let alerts = a.status_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].what, "blocked");
        assert!(alerts[0].body.contains("shosha › backend"));
        assert!(a.status_alerts().is_empty(), "same state, no second alert");
    }

    // ---- W3: tabs and splits ----------------------------------------------------

    fn prefix_key(a: &mut WsApp, k: KeyEvent) -> Option<Out> {
        a.on_key(ctrl_space());
        a.on_key(k)
    }

    fn pk(a: &mut WsApp, c: char) {
        assert_eq!(prefix_key(a, key(c)), None);
    }

    /// Take the pending updates, apply them to the document, and deliver it back.
    fn commit(a: &mut WsApp) -> Vec<WsOp> {
        let mut ops = Vec::new();
        let mut rest = Vec::new();
        for act in std::mem::take(&mut a.actions) {
            match act {
                Action::Update(o) => ops.extend(o),
                other => rest.push(other),
            }
        }
        let mut doc = a.doc.clone();
        for op in &ops {
            apply(&mut doc, op).unwrap();
        }
        a.actions = rest;
        a.on_doc(doc);
        ops
    }

    /// w1 open and attached, focus in the workspace.
    fn open_w1() -> WsApp {
        let mut a = app();
        open_attached(&mut a, "w1");
        a.focus = Focus::Workspace;
        a
    }

    fn focused(a: &WsApp) -> String {
        a.focused_pane().unwrap()
    }

    /// `n-w1` | (`b` over `c`), all attached; focus on `c` after the splits.
    fn three_panes() -> WsApp {
        let mut a = open_w1();
        pk(&mut a, 'v');
        commit(&mut a);
        let b = focused(&a);
        attach_pending(&mut a);
        pk(&mut a, 's');
        commit(&mut a);
        attach_pending(&mut a);
        assert_ne!(b, focused(&a));
        a
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn neighbor_picks_the_nearest_overlapping_pane() {
        // A B C in a row: the nearest wins.
        let row = vec![("a".to_string(), rect(0, 0, 10, 10)), ("b".into(), rect(10, 0, 10, 10)), ("c".into(), rect(20, 0, 10, 10))];
        assert_eq!(neighbor(&row, "a", Nav::Right).as_deref(), Some("b"));
        assert_eq!(neighbor(&row, "c", Nav::Left).as_deref(), Some("b"));
        assert_eq!(neighbor(&row, "a", Nav::Left), None);
        assert_eq!(neighbor(&row, "a", Nav::Up), None);
        assert_eq!(neighbor(&row, "a", Nav::Down), None);
        assert_eq!(neighbor(&row, "zzz", Nav::Down), None);

        // Uneven: a tall left pane next to two stacked ones of different heights.
        let l = vec![("l".to_string(), rect(0, 0, 50, 40)), ("r1".into(), rect(50, 0, 50, 10)), ("r2".into(), rect(50, 10, 50, 30))];
        assert_eq!(neighbor(&l, "l", Nav::Right).as_deref(), Some("r2"), "most overlap");
        assert_eq!(neighbor(&l, "r1", Nav::Left).as_deref(), Some("l"));
        assert_eq!(neighbor(&l, "r2", Nav::Up).as_deref(), Some("r1"));
        assert_eq!(neighbor(&l, "r1", Nav::Down).as_deref(), Some("r2"));
        assert_eq!(neighbor(&l, "r2", Nav::Right), None);

        // Equal overlap: the first one (top / left).
        let even = vec![("l".to_string(), rect(0, 0, 50, 40)), ("t".into(), rect(50, 0, 50, 20)), ("u".into(), rect(50, 20, 50, 20))];
        assert_eq!(neighbor(&even, "l", Nav::Right).as_deref(), Some("t"));

        // Staggered columns: moving down from the wide pane needs an overlap.
        let st = vec![
            ("top".to_string(), rect(0, 0, 100, 10)),
            ("x".into(), rect(0, 10, 30, 30)),
            ("y".into(), rect(30, 10, 70, 30)),
        ];
        assert_eq!(neighbor(&st, "top", Nav::Down).as_deref(), Some("y"));
        assert_eq!(neighbor(&st, "x", Nav::Right).as_deref(), Some("y"));
        assert_eq!(neighbor(&st, "y", Nav::Up).as_deref(), Some("top"));
    }

    #[test]
    fn focus_keys_follow_the_layout_and_persist() {
        let mut a = three_panes();
        let c = focused(&a);
        let ids: Vec<String> = a.active().unwrap().1.layout.panes().iter().map(|p| p.id.clone()).collect();
        // a | (b over c), focus on c.
        assert_eq!(ids[0], "n-w1");
        let b = ids[1].clone();
        pk(&mut a, 'k');
        assert_eq!(focused(&a), b, "up from c");
        assert!(matches!(&a.actions[..], [Action::Update(ops)] if ops == &[WsOp::SetFocus { pane: b.clone() }]));
        a.actions.clear();
        pk(&mut a, 'h');
        assert_eq!(focused(&a), "n-w1");
        a.actions.clear();
        pk(&mut a, 'k');
        assert_eq!(focused(&a), "n-w1", "nothing above: stays");
        assert!(a.actions.is_empty(), "no edit when nothing moves");
        // Arrow keys do the same.
        assert_eq!(prefix_key(&mut a, code(KeyCode::Right)), None);
        assert!(focused(&a) == b || focused(&a) == c);
        let now = focused(&a);
        a.on_key(ctrl_space());
        a.on_key(code(KeyCode::Down));
        assert_ne!(focused(&a), "n-w1");
        let _ = now;
        // The focus change is not a layout change: no pane opens or closes.
        assert!(a.actions.iter().all(|x| matches!(x, Action::Update(_))));
    }

    #[test]
    fn split_and_tab_keys_make_the_right_ops() {
        let mut a = open_w1();
        pk(&mut a, 'v');
        let ops = commit(&mut a);
        let [WsOp::SplitPane { pane, dir, new_id }] = &ops[..] else { panic!("{ops:?}") };
        assert_eq!((pane.as_str(), *dir), ("n-w1", Dir::Horizontal));
        // The new pane takes focus and opens as a shell.
        assert_eq!(focused(&a), *new_id);
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Start { pane, params }] if pane == new_id && params.line.is_none()));

        pk(&mut a, 's');
        let ops = commit(&mut a);
        assert!(matches!(&ops[..], [WsOp::SplitPane { dir: Dir::Vertical, .. }]));

        pk(&mut a, 't');
        let ops = commit(&mut a);
        let [WsOp::AddTab { workspace, name, .. }] = &ops[..] else { panic!("{ops:?}") };
        assert_eq!((workspace.as_str(), name.as_str()), ("w1", "shell 2"), "\"shell\" is taken");
        assert_eq!(a.active().unwrap().1.name, "shell 2");
    }

    #[test]
    fn asking_for_a_command_saves_it_on_the_pane() {
        let mut a = open_w1();
        pk(&mut a, 'T');
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert!(f.title.contains("command"));
        for c in "nvim .".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        assert!(a.dialog.is_none());
        let ops = commit(&mut a);
        let [WsOp::AddTab { name, pane_id, .. }, WsOp::SetCommand { pane, command }] = &ops[..] else { panic!("{ops:?}") };
        assert_eq!(name, "nvim");
        assert_eq!((pane, command.as_deref()), (pane_id, Some("nvim .")));
        // The pane starts through the shell line.
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Start { params, .. }] if params.line.as_deref() == Some("nvim .")));

        // Empty means a shell: no SetCommand.
        pk(&mut a, 'T');
        a.on_key(code(KeyCode::Enter));
        let ops = commit(&mut a);
        assert!(matches!(&ops[..], [WsOp::AddTab { .. }]), "{ops:?}");

        // V / S ask too, and Esc cancels.
        attach_pending(&mut a);
        pk(&mut a, 'V');
        for c in "claude".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        let ops = commit(&mut a);
        let [WsOp::SplitPane { dir, new_id, .. }, WsOp::SetCommand { pane, command }] = &ops[..] else { panic!("{ops:?}") };
        assert_eq!((*dir, pane, command.as_deref()), (Dir::Horizontal, new_id, Some("claude")));
        pk(&mut a, 'S');
        a.on_key(code(KeyCode::Esc));
        assert!(a.dialog.is_none() && a.actions.iter().all(|x| !matches!(x, Action::Update(_))));
        pk(&mut a, 'S');
        a.on_key(key('x'));
        a.on_key(code(KeyCode::Enter));
        let ops = commit(&mut a);
        assert!(matches!(&ops[..], [WsOp::SplitPane { dir: Dir::Vertical, .. }, WsOp::SetCommand { .. }]));
    }

    #[test]
    fn tab_names_are_made_unique() {
        let mut a = open_w1();
        for _ in 0..2 {
            pk(&mut a, 'T');
            for c in "sh -c 'x'".chars() {
                a.on_key(key(c));
            }
            a.on_key(code(KeyCode::Enter));
            commit(&mut a);
        }
        let names: Vec<_> = a.active().map(|(w, _)| w.tabs.iter().map(|t| t.name.clone()).collect()).unwrap();
        assert_eq!(names, ["shell", "sh", "sh 2"]);
    }

    #[test]
    fn resize_keys_send_resize_ops() {
        let mut a = three_panes();
        let tab = a.active().unwrap().1.id.clone();
        let pane = focused(&a);
        for (k, dir, sign) in [('L', Dir::Horizontal, 1), ('H', Dir::Horizontal, -1), ('J', Dir::Vertical, 1), ('K', Dir::Vertical, -1)] {
            pk(&mut a, k);
            let ops = commit(&mut a);
            let [WsOp::ResizePane { tab: t, pane: p, dir: d, delta }] = &ops[..] else { panic!("{ops:?}") };
            assert_eq!((t, p, *d), (&tab, &pane, dir));
            assert_eq!(delta.signum(), sign);
        }
    }

    #[test]
    fn a_resize_reaches_the_streams_and_the_parsers() {
        let mut a = three_panes();
        a.actions.clear();
        pk(&mut a, 'H'); // narrower, so the layout changes
        let before: HashMap<String, (u16, u16)> = a.slots.iter().map(|(k, s)| (k.clone(), s.size)).collect();
        commit(&mut a);
        let resized: Vec<_> = a.actions.iter().filter(|x| matches!(x, Action::Resize { .. })).collect();
        assert!(!resized.is_empty(), "{:?}", a.actions);
        for (id, slot) in &a.slots {
            let parser = slot.view.as_ref().unwrap().parser.screen().size();
            assert_eq!((parser.1.get(), parser.0.get()), slot.size, "{id}: parser follows the stream");
        }
        assert_ne!(before, a.slots.iter().map(|(k, s)| (k.clone(), s.size)).collect::<HashMap<_, _>>());
        a.actions.clear();

        // The terminal got smaller: every pane is told once.
        a.set_screen(rect(0, 0, 80, 20));
        let n = a.actions.iter().filter(|x| matches!(x, Action::Resize { .. })).count();
        assert_eq!(n, 3);
        a.actions.clear();
        a.set_screen(rect(0, 0, 80, 20));
        assert!(a.actions.is_empty(), "same size: nothing to do");
        // Sizes are always the inner cells of the rects.
        for (id, r) in a.pane_rects() {
            assert_eq!(a.slots[&id].size, (r.width - 2, r.height - 2));
        }
    }

    #[test]
    fn tab_keys_switch_and_wrap() {
        let mut a = open_w1();
        pk(&mut a, 't');
        commit(&mut a);
        pk(&mut a, 't');
        commit(&mut a);
        assert_eq!(a.active().unwrap().0.active_tab, 2);
        attach_pending(&mut a);
        pk(&mut a, '1');
        assert!(matches!(&a.actions[0], Action::Update(ops) if ops == &[WsOp::SetActiveTab { workspace: "w1".into(), index: 0 }]));
        assert_eq!(a.active().unwrap().0.active_tab, 0, "switches at once");
        pk(&mut a, '[');
        assert_eq!(a.active().unwrap().0.active_tab, 2, "wraps back");
        pk(&mut a, ']');
        assert_eq!(a.active().unwrap().0.active_tab, 0, "and forward");
        pk(&mut a, '2');
        assert_eq!(a.active().unwrap().0.active_tab, 1);
        a.actions.clear();
        pk(&mut a, '9');
        assert!(a.actions.is_empty());
        assert!(a.current_flash().is_some_and(|(m, e)| e && m.contains("no tab 9")));
        assert_eq!(a.active().unwrap().0.active_tab, 1);
    }

    #[test]
    fn switching_tabs_detaches_and_reattaches_the_same_terminals() {
        let mut a = three_panes();
        let first: Vec<String> = a.active().unwrap().1.layout.panes().iter().map(|p| p.id.clone()).collect();
        // The loop records started terminals in the doc; do the same here.
        let mut doc = a.doc.clone();
        for p in &first {
            apply(&mut doc, &WsOp::SetTerminal { pane: p.clone(), terminal: Some(format!("t-{p}")) }).unwrap();
            a.set.terminals.insert(format!("t-{p}"), terminal(&format!("t-{p}"), true));
        }
        a.on_doc(doc);
        a.actions.clear();
        pk(&mut a, 't');
        commit(&mut a);
        let mut detached: Vec<String> = a.actions.iter().filter_map(|x| if let Action::Detach(p) = x { Some(p.clone()) } else { None }).collect();
        detached.sort();
        let mut want = first.clone();
        want.sort();
        assert_eq!(detached, want, "all three old panes close their streams");
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Start { .. }]), "the new tab starts a shell");
        attach_pending(&mut a);

        // Back to tab 1: all three attach to the recorded terminals.
        pk(&mut a, '1');
        let o = opens(&mut a);
        let mut addrs: Vec<String> = o.iter().map(|p| match p { Plan::Attach { addr, .. } => addr.clone(), other => panic!("{other:?}") }).collect();
        addrs.sort();
        let mut want: Vec<String> = first.iter().map(|p| format!("t-{p}")).collect();
        want.sort();
        assert_eq!(addrs, want);
    }

    #[test]
    fn a_tab_with_missing_terminals_attaches_the_live_and_starts_the_rest() {
        let mut a = three_panes();
        let panes: Vec<String> = a.active().unwrap().1.layout.panes().iter().map(|p| p.id.clone()).collect();
        let mut doc = a.doc.clone();
        for (i, p) in panes.iter().enumerate() {
            apply(&mut doc, &WsOp::SetTerminal { pane: p.clone(), terminal: Some(format!("t{i}")) }).unwrap();
        }
        apply(&mut doc, &WsOp::SetCommand { pane: panes[2].clone(), command: Some("nvim .".into()) }).unwrap();
        a.set.terminals.insert("t0".into(), terminal("t0", true));
        a.set.terminals.insert("t1".into(), terminal("t1", false));
        // t2 is not known at all.
        a.on_doc(doc);
        // Leave the workspace and come back.
        a.open_workspace("w2");
        attach_pending(&mut a);
        a.open_workspace("w1");
        let o = opens(&mut a);
        // The ended pane needs no stream: it shows why it ended.
        assert_eq!(o.len(), 2);
        assert!(matches!(&o[0], Plan::Attach { addr, .. } if addr == "t0"));
        assert!(matches!(&o[1], Plan::Start { params, .. } if params.line.as_deref() == Some("nvim .")), "lost: its command");
        let ended = a.slots[&panes[1]].view.as_ref().unwrap();
        assert!(ended.ended.is_some() && ended.addr == "t1", "finished on its own: stays ended");
    }

    // ---- W4 -------------------------------------------------------------------

    #[test]
    fn restart_rule() {
        assert_eq!(fate(None), Fate::Missing);
        assert_eq!(fate(Some(&terminal("t", true))), Fate::Running);
        for r in ["server_stop", "host_shutdown", "holder_lost", "stopped_by_restart"] {
            assert_eq!(fate(Some(&finished("t", r, None))), Fate::Missing, "{r}");
        }
        assert_eq!(fate(Some(&finished("t", "exited", Some(2)))), Fate::Ended("exited (code 2)".into()));
        assert_eq!(fate(Some(&finished("t", "killed", None))), Fate::Ended("killed".into()));
        // No reason at all: leave it alone rather than guess.
        assert!(matches!(fate(Some(&terminal("t", false))), Fate::Ended(_)));
    }

    /// Workspace w1 with two tabs: tab 1 (open) has 1 pane, tab 2 has 2.
    /// Every pane records a terminal `t-<pane>`; the set is empty.
    fn two_tabs() -> (WsApp, Vec<String>) {
        let mut a = app();
        a.open_workspace("w1");
        attach_pending(&mut a);
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::AddTab { workspace: "w1".into(), id: "t2".into(), name: "two".into(), pane_id: "x1".into() }).unwrap();
        apply(&mut doc, &WsOp::SplitPane { pane: "x1".into(), dir: Dir::Horizontal, new_id: "x2".into() }).unwrap();
        apply(&mut doc, &WsOp::SetCommand { pane: "x2".into(), command: Some("nvim .".into()) }).unwrap();
        apply(&mut doc, &WsOp::SetActiveTab { workspace: "w1".into(), index: 0 }).unwrap();
        let panes = ["n-w1", "x1", "x2"].map(String::from).to_vec();
        for p in &panes {
            apply(&mut doc, &WsOp::SetTerminal { pane: p.clone(), terminal: Some(format!("t-{p}")) }).unwrap();
        }
        a.on_doc(doc);
        // Forget what loading the doc already asked for: tests start from a clean slate.
        a.actions.clear();
        a.starting.clear();
        a.recent.clear();
        a.slots.clear();
        (a, panes)
    }

    fn starts(a: &mut WsApp) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut a.actions)
            .into_iter()
            .filter_map(|x| match x {
                Action::Start { pane, params } => Some((pane, params.line)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn restore_starts_hidden_panes_without_attaching_and_never_twice() {
        let (mut a, _) = two_tabs();
        a.actions.clear();
        // Nothing is known: the visible pane gets a stream, the hidden two are started in the background.
        a.slots.clear();
        a.sync();
        let acts = std::mem::take(&mut a.actions);
        let mut started: Vec<_> = acts
            .iter()
            .filter_map(|x| match x {
                Action::Start { pane, params } => Some((pane.clone(), params.line.clone())),
                _ => None,
            })
            .collect();
        started.sort();
        assert_eq!(started, [("x1".to_string(), None), ("x2".to_string(), Some("nvim .".to_string()))]);
        let opened: Vec<_> = acts.iter().filter(|x| matches!(x, Action::Open { .. })).collect();
        assert_eq!(opened.len(), 1, "only the visible tab gets attach streams");
        assert!(a.slots.keys().all(|p| p == "n-w1"));

        // Syncing again while they start does nothing.
        a.sync();
        a.sync();
        assert!(starts(&mut a).is_empty());

        // One finished; the terminal list does not show it yet: still nothing.
        a.on_started("x1", Ok("t-x1".into()));
        assert!(starts(&mut a).is_empty());
        // The other one failed: not retried in a loop.
        a.on_started("x2", Err("boom".into()));
        a.sync();
        assert!(starts(&mut a).is_empty());
        assert!(a.current_flash().is_some_and(|(m, e)| e && m.contains("boom")));
    }

    #[test]
    fn restore_waits_for_the_first_snapshot_and_skips_running_and_ended() {
        let (mut a, _) = two_tabs();
        a.actions.clear();
        a.synced = false;
        a.sync();
        assert!(a.actions.is_empty(), "before the snapshot every terminal looks missing");
        a.set.terminals.insert("t-x1".into(), terminal("t-x1", true));
        a.set.terminals.insert("t-x2".into(), finished("t-x2", "killed", None));
        a.set.terminals.insert("t-n-w1".into(), terminal("t-n-w1", true));
        a.synced = true;
        a.sync();
        assert!(starts(&mut a).is_empty(), "one runs, one was killed on purpose");
        // After a reboot the killed-by-system one comes back.
        a.set.terminals.insert("t-x2".into(), finished("t-x2", "host_shutdown", None));
        a.sync();
        assert_eq!(starts(&mut a), [("x2".to_string(), Some("nvim .".to_string()))]);
    }

    #[test]
    fn restore_off_starts_nothing_until_enter() {
        let (mut a, _) = two_tabs();
        a.auto_restore = false;
        a.actions.clear();
        a.slots.clear();
        a.sync();
        assert!(a.actions.is_empty(), "{:?}", a.actions);
        let slot = &a.slots["n-w1"];
        assert!(slot.idle && slot.view.is_none());
        a.focus = Focus::Workspace;
        assert_eq!(a.on_key(code(KeyCode::Enter)), None);
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Start { pane, .. }] if pane == "n-w1"));
        // Switching to the hidden tab leaves its panes idle too.
        pk(&mut a, '2');
        assert!(a.slots.values().all(|s| s.idle));
    }

    #[test]
    fn delete_asks_about_the_terminals() {
        let (mut a, panes) = two_tabs();
        for p in &panes {
            a.set.terminals.insert(format!("t-{p}"), terminal(&format!("t-{p}"), true));
        }
        a.slots.clear();
        a.sync();
        a.actions.clear();
        a.focus = Focus::Sidebar;
        a.sel = "w1".into();
        // y: end the terminals, then delete.
        a.on_key(key('D'));
        a.on_key(key('y'));
        let acts = std::mem::take(&mut a.actions);
        let Action::Kill(addrs) = &acts[0] else { panic!("{acts:?}") };
        let mut addrs = addrs.clone();
        addrs.sort();
        assert_eq!(addrs, ["t-n-w1", "t-x1", "t-x2"]);
        assert!(matches!(&acts[1], Action::Update(ops) if ops == &[WsOp::DeleteWorkspace { id: "w1".into() }]));
        assert_eq!(acts.len(), 2);
        // k: only delete.
        a.on_key(key('D'));
        a.on_key(key('k'));
        assert_eq!(only_update(&mut a), [WsOp::DeleteWorkspace { id: "w1".into() }]);
        // n and Esc: nothing.
        for c in [key('n'), code(KeyCode::Esc)] {
            a.on_key(key('D'));
            a.on_key(c);
            assert!(a.dialog.is_none() && a.actions.is_empty());
        }
        // A project kills the terminals of all its workspaces.
        a.sel = "p1".into();
        a.on_key(key('D'));
        a.on_key(key('y'));
        let acts = std::mem::take(&mut a.actions);
        assert!(matches!(&acts[0], Action::Kill(v) if v.len() == 3));
        assert!(matches!(&acts[1], Action::Update(ops) if ops == &[WsOp::DeleteProject { id: "p1".into() }]));
        // `k` means nothing in other questions.
        a.focus = Focus::Workspace;
        a.on_key(ctrl_space());
        a.on_key(key('x'));
        assert!(matches!(a.dialog, Some(Dialog::Confirm { .. })) || a.dialog.is_none());
    }

    #[test]
    fn menu_moves_and_picks_a_view() {
        let mut a = app();
        a.focus = Focus::Workspace;
        a.on_key(ctrl_space());
        a.on_key(key('m'));
        assert!(matches!(a.dialog, Some(Dialog::Menu(0))));
        a.on_key(key('j'));
        a.on_key(code(KeyCode::Down));
        assert!(matches!(a.dialog, Some(Dialog::Menu(2))));
        a.on_key(key('k'));
        a.on_key(code(KeyCode::Enter));
        assert!(a.dialog.is_none());
        assert!(matches!(&a.actions[..], [Action::Board(BoardStart::Tasks)]));
        a.actions.clear();
        // The list stops at both ends, and the last entry is Broadcast.
        a.on_key(ctrl_space());
        a.on_key(key('m'));
        for _ in 0..10 {
            a.on_key(key('j'));
        }
        a.on_key(code(KeyCode::Enter));
        assert!(matches!(&a.actions[..], [Action::Board(BoardStart::Broadcast)]));
        a.actions.clear();
        // Esc closes without a board.
        a.on_key(ctrl_space());
        a.on_key(key('m'));
        a.on_key(code(KeyCode::Esc));
        assert!(a.dialog.is_none() && a.actions.is_empty());
    }

    #[test]
    fn suspend_drops_the_streams_and_sync_reattaches_the_same_terminals() {
        let (mut a, _) = two_tabs();
        a.set.terminals.insert("t-n-w1".into(), terminal("t-n-w1", true));
        a.slots.clear();
        a.sync();
        attach_pending(&mut a);
        a.suspend();
        assert!(a.slots.is_empty());
        a.sync();
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Attach { addr, .. }] if addr == "t-n-w1"));
    }

    #[test]
    fn help_scrolls_and_any_other_key_closes() {
        let mut a = app();
        a.set_screen(Rect::new(0, 0, 80, 24));
        a.dialog = Some(Dialog::Help(0));
        a.on_key(key('j'));
        assert!(matches!(a.dialog, Some(Dialog::Help(1))));
        a.on_key(key('G'));
        let Some(Dialog::Help(end)) = a.dialog else { panic!() };
        assert_eq!(end, ws_ui::help_lines("").len() - 20, "the last line is visible on 24 rows");
        a.on_key(key('x'));
        assert!(a.dialog.is_none());
    }

    #[test]
    fn zoom_fills_the_area_and_toggles() {
        let mut a = three_panes();
        a.actions.clear();
        let f = focused(&a);
        assert_eq!(a.pane_rects().len(), 3);
        pk(&mut a, 'z');
        assert!(a.zoomed());
        let r = a.pane_rects();
        assert_eq!(r, [(f.clone(), a.panes_area)]);
        // Only the zoomed pane is resized; the others keep their streams.
        assert!(matches!(&a.actions[..], [Action::Resize { pane, .. }] if *pane == f), "{:?}", a.actions);
        assert!(a.slots.len() == 3);
        a.actions.clear();
        pk(&mut a, 'z');
        assert!(!a.zoomed());
        assert_eq!(a.pane_rects().len(), 3);
        assert!(matches!(&a.actions[..], [Action::Resize { pane, .. }] if *pane == f));
        assert!(!a.actions.iter().any(|x| matches!(x, Action::Update(_))), "zoom is not saved");

        // Moving the focus or switching tabs ends the zoom.
        pk(&mut a, 'z');
        pk(&mut a, 'h');
        assert!(!a.zoomed());
        pk(&mut a, 'z');
        pk(&mut a, 't');
        commit(&mut a);
        assert!(!a.zoomed());
    }

    #[test]
    fn closing_a_pane_asks_then_kills_and_closes() {
        let mut a = three_panes();
        let f = focused(&a);
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetTerminal { pane: f.clone(), terminal: Some("t77".into()) }).unwrap();
        a.on_doc(doc);
        a.actions.clear();
        pk(&mut a, 'x');
        let Some(Dialog::Confirm { message, .. }) = &a.dialog else { panic!() };
        assert!(message.contains("will end"), "{message}");
        assert!(a.actions.is_empty());
        a.on_key(key('q'));
        assert!(a.dialog.is_some(), "other keys don't answer, and q does not quit");
        a.on_key(key('n'));
        assert!(a.dialog.is_none() && a.actions.is_empty());

        pk(&mut a, 'x');
        a.on_key(key('y'));
        assert!(matches!(&a.actions[..], [Action::Kill(t), Action::Update(ops)]
            if t == &["t77".to_string()] && ops == &[WsOp::ClosePane { pane: f.clone() }]), "{:?}", a.actions);
        commit(&mut a);
        // The pane is gone and its stream closes; the focus moved on.
        assert!(a.actions.iter().any(|x| matches!(x, Action::Detach(p) if *p == f)));
        assert_ne!(focused(&a), f);
        assert!(!a.slots.contains_key(&f));
    }

    #[test]
    fn closing_the_last_pane_of_the_last_tab_shows_the_error() {
        let mut a = open_w1();
        pk(&mut a, 'x');
        assert!(a.dialog.is_none(), "no point asking");
        assert!(a.current_flash().is_some_and(|(m, e)| e && m.contains("delete the workspace")));
        pk(&mut a, 'X');
        assert!(a.dialog.is_none());
        assert!(a.actions.is_empty());
    }

    #[test]
    fn closing_the_last_pane_of_a_tab_closes_the_tab() {
        let mut a = open_w1();
        pk(&mut a, 't');
        commit(&mut a);
        attach_pending(&mut a);
        let only = focused(&a);
        pk(&mut a, 'x');
        a.on_key(key('y'));
        // Nothing was started yet, so there is nothing to kill.
        assert!(matches!(&a.actions[..], [Action::Update(ops)] if ops == &[WsOp::ClosePane { pane: only.clone() }]));
        commit(&mut a);
        assert_eq!(a.active().unwrap().0.tabs.len(), 1);
    }

    #[test]
    fn closing_a_tab_kills_all_its_terminals() {
        let mut a = three_panes();
        let (tab, panes) = {
            let (_, t) = a.active().unwrap();
            (t.id.clone(), t.layout.panes().iter().map(|p| p.id.clone()).collect::<Vec<_>>())
        };
        let mut doc = a.doc.clone();
        for (i, p) in panes.iter().enumerate().take(2) {
            apply(&mut doc, &WsOp::SetTerminal { pane: p.clone(), terminal: Some(format!("t{i}")) }).unwrap();
        }
        a.on_doc(doc);
        pk(&mut a, 't');
        commit(&mut a);
        // Back on tab 1.
        pk(&mut a, '1');
        a.actions.clear();
        pk(&mut a, 'X');
        let Some(Dialog::Confirm { message, .. }) = &a.dialog else { panic!() };
        assert!(message.contains("3 pane"), "{message}");
        a.on_key(key('y'));
        let [Action::Kill(t), Action::Update(ops)] = &a.actions[..] else { panic!("{:?}", a.actions) };
        assert_eq!(t, &["t0".to_string(), "t1".to_string()]);
        assert_eq!(ops, &[WsOp::CloseTab { id: tab }]);
    }

    #[test]
    fn rename_tab_form() {
        let mut a = open_w1();
        pk(&mut a, 'r');
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert_eq!(f.fields[0].input.value(), "shell");
        for _ in 0..5 {
            a.on_key(code(KeyCode::Backspace));
        }
        for c in "editor".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        let ops = commit(&mut a);
        assert!(matches!(&ops[..], [WsOp::RenameTab { name, .. }] if name == "editor"));
        assert_eq!(a.active().unwrap().1.name, "editor");
    }

    #[test]
    fn pane_keys_need_an_open_workspace() {
        let mut a = app();
        for c in ['t', 'T', 'v', 's', 'V', 'S', 'r', 'x', 'X'] {
            pk(&mut a, c);
            assert!(a.dialog.is_none() && a.actions.is_empty(), "{c}");
            assert!(a.current_flash().is_some_and(|(m, _)| m.contains("open a workspace")), "{c}");
        }
        // Moving, resizing and zooming quietly do nothing.
        for c in ['h', 'j', 'k', 'l', 'H', 'J', 'K', 'L', 'z', '1', '[', ']'] {
            pk(&mut a, c);
        }
        assert!(a.actions.is_empty());
    }

    #[test]
    fn keys_and_paste_go_only_to_the_focused_pane() {
        let mut a = three_panes();
        let f = focused(&a);
        assert_eq!(a.on_key(key('a')), Some(Out::Key(key('a'))));
        assert_eq!(a.on_paste("hi").as_deref(), Some("hi"));
        // The loop sends to `focused_pane`, and that follows the doc.
        pk(&mut a, 'h');
        assert_ne!(focused(&a), f);
        // Only the focused slot shows a cursor: the UI asks `focused_pane`.
        assert_eq!(a.focused_pane().unwrap(), "n-w1");
    }
}
