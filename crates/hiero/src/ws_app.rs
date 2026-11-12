//! State and key handling of the workspace screen: the sidebar, the prefix
//! key, dialogs, and which terminal the pane shows. No I/O here: the event
//! loop (`ws_tui`) carries out the [`Action`]s this asks for, so all of it
//! can be tested.

use crate::copy::{Copy, Step};
use crate::notify::Alert;
use crate::paths::{self, expand_home};
use crate::widgets::{PaneView, TextInput};
use crate::config::Prefix;
use crate::keys::{self, MouseAct};
use crate::workspace::{self, Border, Dir, Move, Tab, Workspace, WsDoc, WsOp, WsStatus, new_id};
use crate::ws_ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use hc_client::{EventData, RunParams, Terminal, TerminalSet, WatchMsg};
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

pub const LABEL_WS: &str = "hiero.ws";
/// How long a program may run without its pane before it is ended.
const ORPHAN_AFTER: Duration = Duration::from_secs(5);
pub const LABEL_PANE: &str = "hiero.pane";
/// Set in every pane's environment, so a `hiero` started inside a pane can
/// tell it would be nesting.
pub const ENV_PANE: &str = "HIERO_PANE";
pub const ENV_WS: &str = "HIERO_WS";
pub const LABEL_PROJECT: &str = "hiero.project";

/// How much one `H` `J` `K` `L` press moves a border, in percent of the split.
const RESIZE_STEP: i16 = 5;

/// Arrow keys sent for one wheel notch to a full-screen program that doesn't take the mouse.
const WHEEL_LINES: usize = 3;

/// X10 mode (`?9`) reports presses only, without modifiers.
fn mods_for(s: &vt100::Screen, mods: KeyModifiers) -> KeyModifiers {
    if s.mouse_protocol_mode() == vt100::MouseProtocolMode::Press { KeyModifiers::NONE } else { mods }
}

/// How many columns `<` `>` move the sidebar edge.
const SIDEBAR_STEP: u16 = 4;

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
    /// Tell the pane's program its size in-band (`CSI 48 ; rows ; cols ; 0 ; 0 t`),
    /// for a program that turned on mode 2048: neovim then ignores SIGWINCH and
    /// waits for this report, which the terminal (Hiero, here) has to send.
    SizeReport { pane: String, cols: u16, rows: u16 },
    /// Kill these terminals and remove them from hc.
    Kill(Vec<String>),
    /// Put this text on the system clipboard.
    Clipboard(String),
    /// Start a pane's terminal in the background (no attach) and record it.
    Start { pane: String, params: RunParams },
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
    // Project variables first, so Hiero's own always win.
    params.env.extend(project.env.clone());
    params.env.insert(ENV_PANE.into(), pane.id.clone());
    params.env.insert(ENV_WS.into(), w.id.clone());
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
    RenamePane { pane: String },
    /// A new tab, asking what it runs first.
    NewTab { workspace: String },
    /// A split of `pane`, asking what the new pane runs first.
    NewSplit { pane: String, dir: Dir },
    /// The environment variables of a project, one `KEY=VALUE` per line.
    Env { project: String },
}

/// What a yes/no question is about.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmKind {
    Delete(String),
    ClosePane(String),
    CloseTab(String),
    /// Restart these panes (their ids) so they get the project's new environment.
    RestartEnv(Vec<String>),
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
}

/// What a key press means for the program in the pane.
#[derive(Debug, PartialEq)]
pub enum Out {
    Key(KeyEvent),
    /// Raw bytes (the prefix key typed twice).
    Bytes(Vec<u8>),
}

/// What is under a screen cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// A sidebar row (project or workspace id).
    Sidebar(String),
    /// A tab of the tabs bar.
    Tab(usize),
    /// A pane. `cell` is pane-local (0-based, inside the border); `None` on
    /// the border or the title.
    Pane { id: String, cell: Option<(u16, u16)> },
    /// The line between two panes, for dragging.
    Border(Border),
    Nothing,
}

/// A mouse button held down, and where it started.
enum Drag {
    /// Started on a pane whose program takes mouse reports: the drag and the
    /// release go to it, wherever the pointer is.
    Pane(String),
    /// Selecting with the pointer in a pane in copy mode.
    Copy(String),
    /// A border being moved. The sizes are applied here as the pointer moves
    /// and sent to the document on release.
    Border { tab: String, border: Border, grab: i32, moved: Option<Vec<u16>> },
}

/// One pane: its stream's screen, or why it has none. The panes of the
/// active tab have one, and so does every pane that was shown before and
/// still runs: it stays attached while another tab or workspace is on
/// screen, because a terminal nobody is attached to goes back to the core's
/// own size (200x120), and what its program prints meanwhile is laid out for
/// that size, not for the pane.
pub struct Slot {
    pub attempt: u64,
    /// Present once attached.
    pub view: Option<PaneView>,
    pub error: Option<String>,
    /// Not started on purpose (`restore = false`): Enter starts it.
    pub idle: bool,
    /// Inner cells (cols, rows) the stream and the parser have.
    pub size: (u16, u16),
    /// The attach stream closed (as opposed to a pane that was never running).
    pub lost: bool,
    /// The pane was resized: take a fresh picture from the core at this time.
    resync_at: Option<Instant>,
    /// The attempt of a fresh attach under way; the view stays up meanwhile.
    resync: Option<u64>,
    /// The screen of that fresh attach, shown from its first output on.
    pending: Option<PaneView>,
    /// In the active tab at the last sync.
    shown: bool,
}

impl Slot {
    /// Attached to a program that still runs.
    fn live(&self) -> bool {
        !self.lost && self.view.as_ref().is_some_and(|v| v.ended.is_none())
    }
}

/// How long a pane's size has to hold still before its screen is taken
/// again from the core (a border drag resizes many times).
const RESYNC_DELAY: Duration = Duration::from_millis(150);

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
    /// Live programs labelled with a pane their workspace no longer has, and
    /// since when (see `sweep_orphans`).
    orphans: HashMap<String, Instant>,
    /// `[workspace] restore`: start missing terminals without being asked.
    pub auto_restore: bool,
    /// Lines of history per pane.
    pub scrollback: usize,
    next_attempt: u64,
    /// The tab (id) whose focused pane fills the area. View only, not saved.
    pub zoom: Option<String>,
    /// Copy mode, on the focused pane.
    pub copy: Option<Copy>,
    /// Where the panes go (below the tabs bar).
    pub panes_area: Rect,
    /// The whole screen, to lay the panes out again when the sidebar changes.
    screen: Rect,
    /// What the panes were last laid out for (screen, banner shown, sidebar columns).
    laid_out: Option<(Rect, bool, u16)>,
    /// Sidebar width in columns, and whether it is hidden (prefix `b`).
    pub sidebar_w: u16,
    pub sidebar_hidden: bool,
    /// The terminal this Hiero runs in (`HC_TERMINAL_ID`), never attached.
    pub self_addr: Option<String>,
    /// A new snapshot came after a lost connection and the panes have not
    /// been checked yet (that waits until requests work again).
    reconnected: bool,
    /// The request connection works (the watcher's is events-only).
    client_up: bool,
    pub actions: Vec<Action>,
    drag: Option<Drag>,
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
    /// A project's new environment was sent: once the document shows it, offer
    /// to restart the terminals that run with the old one.
    pending_env: Option<(String, BTreeMap<String, String>)>,
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
            orphans: HashMap::new(),
            failed: HashSet::new(),
            auto_restore: true,
            scrollback: 5000,
            next_attempt: 0,
            zoom: None,
            copy: None,
            panes_area: ws_ui::areas(Rect::new(0, 0, 100, 30), false, ws_ui::SIDEBAR_W).panes,
            screen: Rect::new(0, 0, 100, 30),
            laid_out: None,
            sidebar_w: ws_ui::SIDEBAR_W,
            sidebar_hidden: false,
            self_addr: None,
            reconnected: false,
            client_up: false,
            actions: Vec::new(),
            drag: None,
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
            pending_env: None,
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
                // Not the first one: the core went away and is back (or events
                // were missed), so what the panes show may be out of date.
                let again = self.connected_once;
                self.set.apply_watch(msg);
                self.disconnected = None;
                self.connected_once = true;
                // A fresh snapshot is a new baseline, not a change.
                self.last_status = self.statuses();
                self.synced = true;
                // Only now do we know which terminals exist, so only now is
                // it safe to decide what is missing.
                if again {
                    self.reconnected = true;
                    self.try_recover();
                } else {
                    self.sync();
                }
                Vec::new()
            }
            WatchMsg::Disconnected { error } => {
                self.client_up = false;
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
                // Only these can change a status or which terminals exist; the
                // rest (title, size, foreground, keep) are frequent and can't.
                // New kinds default to running it.
                let relevant = !matches!(
                    e.data,
                    EventData::Title { .. } | EventData::Size { .. } | EventData::Foreground(_) | EventData::Keep { .. }
                );
                if relevant {
                    // The loop no longer syncs every pass: a terminal that appeared or ended can change what a pane needs.
                    self.sync();
                    self.status_alerts()
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// A fresh copy of the document (first read, reload, or after our own edit).
    pub fn on_doc(&mut self, doc: WsDoc) {
        // Panes on screen whose terminal the document dropped or swapped
        // (`hiero pane split --reuse`): show the pane's new one, started if
        // needed. Decided from the document alone: the old terminal may already
        // have ended, and the pane be busy attaching to it again. Our own
        // edits (a terminal we just started or show) don't count.
        let mut replaced: Vec<String> = self
            .slots
            .iter()
            .filter(|(id, _)| !self.starting.contains(*id))
            .filter_map(|(id, s)| {
                let was = self.doc.pane(id)?.2.terminal.as_ref()?;
                let now = doc.pane(id)?.2.terminal.as_ref();
                let ours = now.is_some_and(|n| s.view.as_ref().is_some_and(|v| &v.addr == n) || self.recent.get(id) == Some(n));
                (now != Some(was) && !ours).then(|| id.clone())
            })
            .collect();
        replaced.sort();
        self.doc = doc;
        for pane in replaced {
            self.restart(&pane, false, false);
        }
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
                None => self.focus = if self.sidebar_hidden { Focus::Workspace } else { Focus::Sidebar },
            }
        }
        if let Some(id) = self.pending_open.take_if(|id| self.doc.workspace(id).is_some()) {
            self.sel = id.clone();
            self.focus = Focus::Workspace;
            self.open_workspace(&id);
        }
        self.offer_restart();
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

    /// The screen changed size (or the banner came or went).
    /// Cheap when none of the screen, the banner and the sidebar changed.
    pub fn set_screen(&mut self, screen: Rect) {
        let key = (screen, self.disconnected.is_some(), self.sidebar_cols());
        if self.laid_out == Some(key) {
            return;
        }
        self.laid_out = Some(key);
        self.screen = screen;
        self.panes_area = ws_ui::areas(screen, self.disconnected.is_some(), self.sidebar_cols()).panes;
        self.sync();
    }

    /// Columns the sidebar takes right now (0 when hidden).
    pub fn sidebar_cols(&self) -> u16 {
        if self.sidebar_hidden { 0 } else { self.sidebar_w }
    }

    /// Apply the remembered sidebar state.
    pub fn set_sidebar(&mut self, width: Option<u16>, hidden: bool) {
        self.sidebar_w = width.unwrap_or(ws_ui::SIDEBAR_W).clamp(ws_ui::SIDEBAR_MIN, ws_ui::SIDEBAR_MAX);
        self.sidebar_hidden = hidden;
        self.relayout();
    }

    /// Lay the panes out again for the sidebar's new width; `sync` then
    /// resizes the streams and the parsers.
    fn relayout(&mut self) {
        self.laid_out = None;
        self.set_screen(self.screen);
    }

    /// Prefix `b`: hide the sidebar (the workspace takes the width) or show it.
    /// A hidden sidebar can't keep the keyboard.
    fn toggle_sidebar(&mut self) {
        self.sidebar_hidden = !self.sidebar_hidden;
        if self.sidebar_hidden && self.focus == Focus::Sidebar {
            self.focus = Focus::Workspace;
        }
        self.relayout();
    }

    /// Prefix `<` `>`: move the sidebar edge. Resizing shows a hidden sidebar.
    fn resize_sidebar(&mut self, wider: bool) {
        let w = if wider { self.sidebar_w.saturating_add(SIDEBAR_STEP) } else { self.sidebar_w.saturating_sub(SIDEBAR_STEP) };
        self.sidebar_w = w.clamp(ws_ui::SIDEBAR_MIN, ws_ui::SIDEBAR_MAX);
        self.sidebar_hidden = false;
        self.relayout();
    }

    /// Once a second. A pane whose program the system took down (not one that
    /// was ended) starts again, but not at once: a core that is stopping marks
    /// its terminals finished a moment before the connection drops, and that
    /// case belongs to the reconnect.
    pub fn on_tick(&mut self) {
        self.sync();
        if self.client_up && self.disconnected.is_none() {
            self.recover_lost(false);
            self.sweep_orphans(Instant::now());
        }
    }

    /// End programs labelled with a pane that their (still existing) workspace
    /// no longer has, on screen or hidden, once they have been like that for
    /// `ORPHAN_AFTER`: a pane closed while the document never recorded its
    /// terminal, for one. A workspace that is gone is left alone: deleting one
    /// can keep its programs on purpose.
    fn sweep_orphans(&mut self, now: Instant) {
        if !self.synced || self.reconnected {
            return;
        }
        let mut still = HashMap::new();
        let mut kill = vec![];
        for t in self.set.terminals.values() {
            if t.state != hc_client::TerminalState::Live || self.self_addr.as_deref() == Some(t.address.as_str()) {
                continue;
            }
            let (Some(ws), Some(pane)) = (t.labels.get(LABEL_WS), t.labels.get(LABEL_PANE)) else { continue };
            let Some((_, w)) = self.doc.workspace(ws) else { continue };
            if w.all_panes().iter().any(|p| &p.id == pane) {
                continue;
            }
            let since = self.orphans.get(&t.address).copied().unwrap_or(now);
            if now.duration_since(since) >= ORPHAN_AFTER {
                kill.push(t.address.clone());
            } else {
                still.insert(t.address.clone(), since);
            }
        }
        self.orphans = still;
        if !kill.is_empty() {
            tracing::info!("ending programs whose pane is gone: {kill:?}");
            self.actions.push(Action::Kill(kill));
        }
    }

    /// Live programs labelled `key` = one of `values`: also the ones the
    /// document lost track of.
    fn labelled(&self, key: &str, values: &[String]) -> Vec<String> {
        self.set
            .terminals
            .values()
            .filter(|t| t.state == hc_client::TerminalState::Live && t.labels.get(key).is_some_and(|v| values.contains(v)))
            .map(|t| t.address.clone())
            .collect()
    }

    /// A connection that can send requests is up (also again after a restart).
    pub fn on_connected(&mut self) {
        self.client_up = true;
        self.try_recover();
    }

    /// Make the open panes match what the document says is on screen: close
    /// the ones that left, open the new ones (attach if the terminal still
    /// runs, else start one), and resize the ones whose rectangle changed.
    /// Safe to call at any time; it only acts on differences.
    pub fn sync(&mut self) {
        self.sync_panes();
        self.check_copy();
    }

    fn sync_panes(&mut self) {
        // Before the first snapshot every terminal looks missing. After a
        // reconnect the same holds until the panes have been checked.
        if !self.synced || self.reconnected {
            return;
        }
        let (panes, tab) = match self.active() {
            Some((_, t)) => (t.layout.panes().iter().map(|p| p.id.clone()).collect::<Vec<_>>(), t.id.clone()),
            None => (vec![], String::new()),
        };
        if self.zoom.as_ref().is_some_and(|z| *z != tab) {
            self.zoom = None;
        }
        // Off screen, a pane keeps its stream while its program runs (see
        // `Slot`); one that never attached, failed or ended starts over when
        // it is shown again.
        let mut gone: Vec<String> = self
            .slots
            .iter()
            .filter(|(id, s)| !panes.contains(id) && !(s.live() && self.doc.has_pane(id)))
            .map(|(id, _)| id.clone())
            .collect();
        gone.sort();
        for id in gone {
            self.slots.remove(&id);
            self.actions.push(Action::Detach(id));
        }
        for (id, slot) in &mut self.slots {
            let shown = panes.contains(id);
            // Back on screen: take the terminal's own picture again, in
            // case ours drifted while nobody looked.
            if shown && !slot.shown && slot.live() {
                slot.resync_at = Some(Instant::now() + RESYNC_DELAY);
            }
            slot.shown = shown;
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
                if let Some(next) = &mut slot.pending {
                    next.resize(size.0, size.1);
                }
                // Our screen and the terminal's now differ: the terminal
                // rewraps long lines and brings history back when it grows,
                // ours only cuts and pads. Take the real one once this settles.
                slot.resync_at = Some(Instant::now() + RESYNC_DELAY);
                self.actions.push(Action::Resize { pane: id.clone(), cols: size.0, rows: size.1 });
                if view.parser.screen().in_band_resize() {
                    self.actions.push(Action::SizeReport { pane: id.clone(), cols: size.0, rows: size.1 });
                }
            }
        }
        self.restore_hidden();
    }

    // ---- copy mode ------------------------------------------------------------

    /// Copy mode and the screen it works on.
    fn copy_parts(&mut self) -> Option<(&mut Copy, &mut vt100::Screen)> {
        let c = self.copy.as_mut()?;
        let view = self.slots.get_mut(&c.pane)?.view.as_mut()?;
        Some((c, view.parser.screen_mut()))
    }

    /// The copy mode of `pane` and its screen, for drawing.
    pub fn copy_of(&self, pane: &str) -> Option<&Copy> {
        self.copy.as_ref().filter(|c| c.pane == pane)
    }

    /// Start copy mode on `pane` (unless it already is). False when the pane
    /// has no screen.
    fn enter_copy(&mut self, pane: &str) -> bool {
        if self.copy_of(pane).is_some() {
            return true;
        }
        self.exit_copy();
        let Some(slot) = self.slots.get_mut(pane) else { return false };
        let (size, attempt) = (slot.size, slot.attempt);
        let Some(view) = slot.view.as_mut() else { return false };
        let mut c = Copy::new(pane.to_string(), size, attempt, view.parser.screen());
        c.apply(view.parser.screen_mut());
        self.copy = Some(c);
        true
    }

    /// Leave copy mode, back to the live screen.
    pub fn exit_copy(&mut self) {
        let Some(c) = self.copy.take() else { return };
        if let Some(view) = self.slots.get_mut(&c.pane).and_then(|s| s.view.as_mut()) {
            view.parser.screen_mut().set_scrollback(0);
        }
    }

    /// Copy mode lasts while its pane has the focus and keeps its size and stream.
    fn check_copy(&mut self) {
        let Some(c) = &self.copy else { return };
        let keep = self.focus == Focus::Workspace
            && self.focused_pane().as_deref() == Some(c.pane.as_str())
            && self.slots.get(&c.pane).is_some_and(|s| s.view.is_some() && s.size == c.size && s.attempt == c.attempt);
        if !keep {
            self.exit_copy();
        }
    }

    fn do_copy(&mut self, step: Step) {
        match step {
            Step::Stay => {}
            Step::Exit => self.exit_copy(),
            Step::Note(m) => self.flash_error(&m),
            Step::Copy(text) => {
                self.exit_copy();
                if text.is_empty() {
                    self.flash_error("nothing to copy");
                    return;
                }
                let n = text.lines().count().max(1);
                self.flash_info(&format!("copied {n} line{}", if n == 1 { "" } else { "s" }));
                self.actions.push(Action::Clipboard(text));
            }
        }
    }

    fn copy_key(&mut self, k: KeyEvent) {
        let Some((c, s)) = self.copy_parts() else {
            self.exit_copy();
            return;
        };
        let step = c.key(s, k);
        self.do_copy(step);
    }

    /// When the next resized pane wants its screen taken again.
    pub fn next_resync(&self) -> Option<Instant> {
        self.slots.values().filter_map(|s| s.resync_at).min()
    }

    /// Attach again to the panes whose resize has settled. The pane keeps
    /// showing its old stream until the new one is open, then swaps; the
    /// first thing a new stream sends is the terminal's real screen.
    pub fn resync_due(&mut self, now: Instant) {
        let mut due: Vec<String> =
            self.slots.iter().filter(|(_, s)| s.resync_at.is_some_and(|t| t <= now)).map(|(id, _)| id.clone()).collect();
        due.sort();
        for pane in due {
            let Some(slot) = self.slots.get_mut(&pane) else { continue };
            slot.resync_at = None;
            let Some(addr) = slot.view.as_ref().filter(|v| v.ended.is_none()).map(|v| v.addr.clone()) else { continue };
            self.next_attempt += 1;
            slot.resync = Some(self.next_attempt);
            let (cols, rows) = slot.size;
            self.actions.push(Action::Open { plan: Plan::Attach { pane, addr }, attempt: self.next_attempt, cols, rows });
        }
    }

    /// The terminal started in this run for `pane` that the terminal list
    /// does not show yet.
    fn recently_started(&self, pane: &str) -> Option<String> {
        let addr = self.recent.get(pane)?;
        (self.terminal_of(pane).as_ref() == Some(addr) && self.set.get(addr).is_none()).then(|| addr.clone())
    }

    /// Terminal starts waiting in `actions`, with or without an attach.
    fn queued_starts(&self) -> usize {
        let starts = |a: &&Action| matches!(a, Action::Start { .. } | Action::Open { plan: Plan::Start { .. }, .. });
        self.actions.iter().filter(starts).count()
    }

    /// After a core restart: check every pane against the new list of
    /// terminals and bring back what the restart rule says. Waits for the
    /// request connection, because starting needs it.
    fn try_recover(&mut self) {
        if !self.reconnected || !self.client_up {
            return;
        }
        self.reconnected = false;
        // The new snapshot is the truth: starts and failures of the old
        // connection say nothing about it.
        self.recent.clear();
        self.failed.clear();
        let before = self.queued_starts();
        self.sync(); // the panes of the other tabs
        self.recover_lost(true); // the panes on screen
        self.sync();
        let n = self.queued_starts() - before;
        if n > 0 {
            self.flash_info(&format!("core restarted — restored {n} pane{}", if n == 1 { "" } else { "s" }));
        }
    }

    /// Panes on screen whose stream closed (or whose open failed while the
    /// core was away, after a reconnect): open them again when the restart
    /// rule says their terminal is gone or was taken down by the system. One
    /// that ended on its own or was killed stays as it is. After a reconnect
    /// (`reconnect`) the terminal list is fresh, so a stream that was only
    /// lost to a still-running terminal attaches again, and a terminal that
    /// is no longer listed counts as gone.
    fn recover_lost(&mut self, reconnect: bool) {
        let mut lost: Vec<String> =
            self.slots.iter().filter(|(_, s)| s.lost || (reconnect && s.error.is_some())).map(|(id, _)| id.clone()).collect();
        lost.sort();
        for pane in lost {
            let t = self.terminal_of(&pane).and_then(|a| self.set.get(&a));
            let go = match fate(t) {
                Fate::Missing => reconnect || t.is_some(),
                Fate::Running => reconnect,
                Fate::Ended(_) => false,
            };
            if go {
                self.restart(&pane, false, false);
            }
        }
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
        // Never show Hiero inside itself: that pane would draw this screen
        // drawing itself. Enter still gives the pane a fresh terminal.
        let p = match p {
            Plan::Attach { pane, addr } if self.self_addr.as_deref() == Some(addr.as_str()) => {
                Plan::Ended { pane, addr, why: "this is Hiero itself".into() }
            }
            p => p,
        };
        self.failed.remove(pane);
        let (cols, rows) = self.size_of(pane);
        self.next_attempt += 1;
        let attempt = self.next_attempt;
        let mut slot =
            Slot { attempt, view: None, error: None, idle: false, size: (cols, rows), lost: false, resync_at: None, resync: None, pending: None, shown: true };
        match &p {
            Plan::Ended { addr, why, .. } => {
                let mut view = PaneView::new(addr.clone(), cols, rows, self.scrollback);
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
    /// `manual` as in `open_slot`: a recovery on its own honours `restore = false`.
    fn restart(&mut self, pane: &str, fresh: bool, manual: bool) {
        if self.starting.contains(pane) {
            return;
        }
        self.slots.remove(pane);
        self.actions.push(Action::Detach(pane.to_string()));
        // Off screen it opens when it is shown, at the size it then has.
        if self.on_screen(pane) {
            self.open_slot(pane, fresh, manual);
        }
    }

    /// An open attempt finished. False when the pane has moved on (another tab
    /// or workspace, or a newer attempt): the caller then closes the stream.
    pub fn on_opened(&mut self, pane: &str, attempt: u64, result: Result<(String, u16, u16), String>) -> bool {
        let was_start = self.starting.remove(pane);
        if was_start && let Ok((addr, ..)) = &result {
            self.recent.insert(pane.to_string(), addr.clone());
        }
        // A fresh attach of a resized pane: swap it in, or keep the old stream.
        if let Some(slot) = self.slots.get_mut(pane).filter(|s| s.resync == Some(attempt)) {
            slot.resync = None;
            let (Ok((addr, cols, rows)), false) = (result, slot.lost) else { return false };
            slot.attempt = attempt;
            // Shown from its first output on, so the pane never blinks empty.
            slot.pending = Some(PaneView::new(addr, cols, rows, self.scrollback));
            if let Some(view) = &mut slot.view {
                view.resize(cols, rows);
            }
            slot.size = (cols, rows);
            // The layout may have changed again while we were attaching.
            self.sync();
            return true;
        }
        let Some(slot) = self.slots.get_mut(pane).filter(|s| s.attempt == attempt) else { return false };
        let ok = result.is_ok();
        match result {
            Ok((addr, cols, rows)) => {
                slot.view = Some(PaneView::new(addr.clone(), cols, rows, self.scrollback));
                slot.size = (cols, rows);
                slot.error = None;
            }
            Err(e) => slot.error = Some(e),
        }
        // The layout may have changed while we were attaching.
        self.sync();
        ok
    }

    /// The pane is in the active tab.
    fn on_screen(&self, pane: &str) -> bool {
        self.active().is_some_and(|(_, t)| t.layout.find(pane).is_some())
    }

    /// Output or the end of a pane's stream. True when the pane is on screen
    /// (there is something new to draw).
    pub fn on_stream(&mut self, pane: &str, msg: hc_client::StreamMsg) -> bool {
        use hc_client::StreamMsg;
        let on_screen = self.on_screen(pane);
        let Some(slot) = self.slots.get_mut(pane) else { return false };
        if matches!(msg, StreamMsg::Data(_))
            && let Some(next) = slot.pending.take()
        {
            slot.view = Some(next);
        }
        if matches!(msg, StreamMsg::Closed(_)) && !on_screen {
            // Nobody is looking: it starts over when it is shown again.
            self.slots.remove(pane);
            self.actions.push(Action::Detach(pane.to_string()));
            return false;
        }
        let Some(view) = slot.view.as_mut() else { return false };
        match msg {
            StreamMsg::Data(bytes) => {
                let before = view.parser.screen().in_band_resize();
                view.parser.process(&bytes);
                // Copy mode keeps showing the same lines while output arrives.
                if let Some(c) = self.copy.as_mut().filter(|c| c.pane == pane) {
                    c.apply(view.parser.screen_mut());
                }
                // Turned on now, or seen in the replay of an attach (which may
                // have resized the terminal under the program): report the size.
                if !before && view.parser.screen().in_band_resize() {
                    let (rows, cols) = view.parser.screen().size();
                    self.actions.push(Action::SizeReport { pane: pane.to_string(), cols: cols.get(), rows: rows.get() });
                }
            }
            StreamMsg::Resize { .. } => {}
            StreamMsg::Closed(reason) => {
                view.ended = Some(crate::widgets::describe_close(reason.as_ref()));
                slot.lost = true;
                slot.pending = None;
            }
        }
        on_screen
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

    pub fn flash_info(&mut self, msg: &str) {
        self.flash = Some(Flash { msg: msg.into(), error: false, at: Instant::now() });
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
        let to_pane = self.focus == Focus::Workspace && !self.armed && self.copy.is_none() && self.pane_live();
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

    // ---- mouse ----------------------------------------------------------------

    /// What is at screen cell (`x`, `y`), from the same geometry the screen
    /// is drawn with. Dialogs are not considered (the caller ignores the mouse then).
    pub fn hit_test(&self, x: u16, y: u16) -> Hit {
        let a = ws_ui::areas(self.screen, self.disconnected.is_some(), self.sidebar_cols());
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        if inside(a.sidebar) {
            let list = ws_ui::sidebar_inner(a.sidebar);
            if !inside(list) {
                return Hit::Nothing;
            }
            let rows = self.rows();
            let cursor = rows.iter().position(|r| r.id() == self.sel).unwrap_or(0);
            let i = ws_ui::sidebar_top(cursor, list.height as usize) + (y - list.y) as usize;
            return rows.get(i).map_or(Hit::Nothing, |r| Hit::Sidebar(r.id().to_string()));
        }
        if inside(a.tabs) {
            let Some((w, _)) = self.active() else { return Hit::Nothing };
            let at = x - a.tabs.x;
            let i = ws_ui::tab_cells(w).iter().position(|&(from, width)| at >= from && at < from + width);
            return i.map_or(Hit::Nothing, Hit::Tab);
        }
        if !inside(self.panes_area) {
            return Hit::Nothing;
        }
        if !self.zoomed()
            && let Some((_, t)) = self.active()
            && let Some(b) = workspace::borders(&t.layout, self.panes_area).into_iter().rev().find(|b| b.contains(x, y))
        {
            return Hit::Border(b);
        }
        for (id, r) in self.pane_rects() {
            if inside(r) {
                let in_border = x > r.x && x + 1 < r.x + r.width && y > r.y && y + 1 < r.y + r.height;
                let cell = in_border.then(|| (x - r.x - 1, y - r.y - 1));
                return Hit::Pane { id, cell };
            }
        }
        Hit::Nothing
    }

    /// The screen of a pane whose program is running, if it asked for mouse reports.
    fn mouse_screen(&self, pane: &str) -> Option<&vt100::Screen> {
        let v = self.slots.get(pane)?.view.as_ref().filter(|v| v.ended.is_none())?;
        let s = v.parser.screen();
        (s.mouse_protocol_mode() != vt100::MouseProtocolMode::None).then_some(s)
    }

    /// A pointer event, or `None`. The answer is what to type into a pane's
    /// program: (pane id, bytes). Not while a dialog is open: those are
    /// keyboard only.
    pub fn on_mouse(&mut self, ev: MouseEvent) -> Option<(String, Vec<u8>)> {
        let out = self.mouse(ev);
        self.sync();
        out
    }

    fn mouse(&mut self, ev: MouseEvent) -> Option<(String, Vec<u8>)> {
        if self.dialog.is_some() {
            self.drag = None;
            return None;
        }
        let (x, y, mods) = (ev.column, ev.row, ev.modifiers);
        match ev.kind {
            MouseEventKind::Down(button) => {
                self.armed = false;
                self.press(button, mods, x, y)
            }
            MouseEventKind::Drag(button) => self.drag_to(button, mods, x, y),
            MouseEventKind::Up(button) => self.release(button, mods, x, y),
            MouseEventKind::Moved => {
                if self.drag.is_some() {
                    return None;
                }
                let Hit::Pane { id, cell: Some((cx, cy)) } = self.hit_test(x, y) else { return None };
                let s = self.mouse_screen(&id)?;
                if s.mouse_protocol_mode() != vt100::MouseProtocolMode::AnyMotion {
                    return None;
                }
                let bytes = keys::encode_mouse(MouseAct::Move, mods, cx, cy, s.mouse_protocol_encoding())?;
                Some((id, bytes))
            }
            MouseEventKind::ScrollUp => self.wheel(MouseAct::WheelUp, mods, x, y),
            MouseEventKind::ScrollDown => self.wheel(MouseAct::WheelDown, mods, x, y),
            MouseEventKind::ScrollLeft => self.wheel(MouseAct::WheelLeft, mods, x, y),
            MouseEventKind::ScrollRight => self.wheel(MouseAct::WheelRight, mods, x, y),
        }
    }

    fn press(&mut self, button: MouseButton, mods: KeyModifiers, x: u16, y: u16) -> Option<(String, Vec<u8>)> {
        self.drag = None;
        let left = button == MouseButton::Left;
        match self.hit_test(x, y) {
            Hit::Sidebar(id) if left => {
                self.sel = id.clone();
                if self.doc.workspace(&id).is_some() {
                    self.open_workspace(&id);
                    self.focus = Focus::Workspace;
                } else {
                    self.focus = Focus::Sidebar;
                    self.toggle_collapse(&id);
                }
            }
            Hit::Tab(i) if left => {
                self.focus = Focus::Workspace;
                self.go_tab(Some(i), 0);
            }
            Hit::Border(border) if left => {
                let (_, t) = self.active()?;
                let v = if border.dir == Dir::Horizontal { x } else { y };
                let grab = border.at as i32 - v as i32;
                self.drag = Some(Drag::Border { tab: t.id.clone(), border, grab, moved: None });
            }
            Hit::Pane { id, cell } => {
                self.focus = Focus::Workspace;
                if self.focused_pane().as_deref() != Some(id.as_str()) {
                    self.edit(WsOp::SetFocus { pane: id.clone() });
                }
                let (cx, cy) = cell?;
                if left && self.copy_of(&id).is_some() {
                    // Copy mode: the click places the cursor, a drag selects.
                    let (c, s) = self.copy_parts()?;
                    c.point(s, cx, cy, false);
                    self.drag = Some(Drag::Copy(id));
                    return None;
                }
                let s = self.mouse_screen(&id)?;
                let bytes = keys::encode_mouse(MouseAct::Press(button), mods_for(s, mods), cx, cy, s.mouse_protocol_encoding());
                self.drag = Some(Drag::Pane(id.clone()));
                return Some((id, bytes?));
            }
            _ => {}
        }
        None
    }

    /// Pointer cell inside the pane's inner area, whether or not the pointer is.
    fn clamp_to(&self, pane: &str, x: u16, y: u16) -> Option<(u16, u16)> {
        let (_, r) = self.pane_rects().into_iter().find(|(id, _)| id == pane)?;
        let (w, h) = Self::inner(r);
        Some((x.saturating_sub(r.x + 1).min(w - 1), y.saturating_sub(r.y + 1).min(h - 1)))
    }

    fn drag_to(&mut self, button: MouseButton, mods: KeyModifiers, x: u16, y: u16) -> Option<(String, Vec<u8>)> {
        match self.drag.as_ref()? {
            Drag::Copy(id) => {
                let id = id.clone();
                let (_, r) = self.pane_rects().into_iter().find(|(p, _)| *p == id)?;
                let (cx, cy) = self.clamp_to(&id, x, y)?;
                let (c, s) = self.copy_parts()?;
                // Past the top or bottom edge: scroll that way, a line a move.
                if y <= r.y {
                    c.scroll(s, -1);
                } else if y + 1 >= r.y + r.height {
                    c.scroll(s, 1);
                }
                c.point(s, cx, cy, true);
                None
            }
            Drag::Pane(id) => {
                let id = id.clone();
                let (cx, cy) = self.clamp_to(&id, x, y)?;
                let s = self.mouse_screen(&id)?;
                if !matches!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::ButtonMotion | vt100::MouseProtocolMode::AnyMotion) {
                    return None;
                }
                let bytes = keys::encode_mouse(MouseAct::Drag(button), mods, cx, cy, s.mouse_protocol_encoding())?;
                Some((id, bytes))
            }
            Drag::Border { tab, border, grab, .. } => {
                let (tab, border) = (tab.clone(), border.clone());
                let v = if border.dir == Dir::Horizontal { x } else { y };
                let pointer = (v as i32 + grab).max(0) as u16;
                let (_, t) = self.doc.tab(&tab)?;
                let sizes = workspace::drag_sizes(&t.layout, &border, pointer)?;
                let op = WsOp::SetSizes { tab, path: border.path.clone(), sizes: sizes.clone() };
                if workspace::apply(&mut self.doc, &op).is_ok()
                    && let Some(Drag::Border { moved, .. }) = &mut self.drag
                {
                    *moved = Some(sizes);
                }
                None
            }
        }
    }

    fn release(&mut self, button: MouseButton, mods: KeyModifiers, x: u16, y: u16) -> Option<(String, Vec<u8>)> {
        match self.drag.take()? {
            Drag::Copy(_) => None,
            Drag::Pane(id) => {
                let (cx, cy) = self.clamp_to(&id, x, y)?;
                let s = self.mouse_screen(&id)?;
                if s.mouse_protocol_mode() == vt100::MouseProtocolMode::Press {
                    return None;
                }
                let bytes = keys::encode_mouse(MouseAct::Release(button), mods, cx, cy, s.mouse_protocol_encoding())?;
                Some((id, bytes))
            }
            Drag::Border { tab, border, moved, .. } => {
                // One document write for the whole drag.
                if let Some(sizes) = moved {
                    self.actions.push(Action::Update(vec![WsOp::SetSizes { tab, path: border.path, sizes }]));
                }
                None
            }
        }
    }

    /// The wheel: a program with mouse reporting gets it, a full-screen one
    /// without gets arrow keys, the sidebar moves its cursor.
    fn wheel(&mut self, act: MouseAct, mods: KeyModifiers, x: u16, y: u16) -> Option<(String, Vec<u8>)> {
        let sb = ws_ui::areas(self.screen, self.disconnected.is_some(), self.sidebar_cols()).sidebar;
        let in_sidebar = x >= sb.x && x < sb.x + sb.width && y >= sb.y && y < sb.y + sb.height;
        match self.hit_test(x, y) {
            _ if in_sidebar => {
                match act {
                    MouseAct::WheelUp => self.move_sel(-1),
                    MouseAct::WheelDown => self.move_sel(1),
                    _ => {}
                }
                None
            }
            Hit::Pane { id, cell: Some((cx, cy)) } => {
                let by = match act {
                    MouseAct::WheelUp => -(WHEEL_LINES as isize),
                    MouseAct::WheelDown => WHEEL_LINES as isize,
                    _ => 0,
                };
                if by != 0 && self.copy_of(&id).is_some() {
                    let (c, s) = self.copy_parts()?;
                    let step = c.scroll(s, by);
                    self.do_copy(step);
                    return None;
                }
                if let Some(s) = self.mouse_screen(&id) {
                    let bytes = keys::encode_mouse(act, mods, cx, cy, s.mouse_protocol_encoding())?;
                    return Some((id, bytes));
                }
                let v = self.slots.get(&id)?.view.as_ref().filter(|v| v.ended.is_none())?;
                let s = v.parser.screen();
                if !s.alternate_screen() {
                    // A plain screen: up into its history, in copy mode.
                    if by >= 0 || s.scrollback_len() == 0 {
                        return None;
                    }
                    self.focus = Focus::Workspace;
                    if self.focused_pane().as_deref() != Some(id.as_str()) {
                        self.edit(WsOp::SetFocus { pane: id.clone() });
                    }
                    if self.enter_copy(&id) {
                        let (c, s) = self.copy_parts()?;
                        c.from_wheel = true;
                        let step = c.scroll(s, by);
                        self.do_copy(step);
                    }
                    return None;
                }
                let code = match act {
                    MouseAct::WheelUp => KeyCode::Up,
                    MouseAct::WheelDown => KeyCode::Down,
                    _ => return None,
                };
                let arrow = keys::encode(KeyEvent::new(code, KeyModifiers::NONE), s.application_cursor())?;
                Some((id, arrow.repeat(WHEEL_LINES)))
            }
            _ => None,
        }
    }

    fn prefix_command(&mut self, k: KeyEvent) -> Option<Out> {
        if self.prefix.matches(&k) {
            return self.pane_live().then(|| Out::Bytes(vec![self.prefix.byte()]));
        }
        match k.code {
            KeyCode::Char('w') => {
                if self.focus == Focus::Sidebar {
                    self.focus = Focus::Workspace;
                } else {
                    self.focus = Focus::Sidebar;
                    // Asking for the sidebar shows it.
                    if self.sidebar_hidden {
                        self.toggle_sidebar();
                    }
                }
            }
            KeyCode::Char('b') => self.toggle_sidebar(),
            KeyCode::Char('<') => self.resize_sidebar(false),
            KeyCode::Char('>') => self.resize_sidebar(true),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help(0)),
            KeyCode::Char('n') => self.add_workspace_form(),
            KeyCode::Char('N') => self.add_project_form(),
            KeyCode::Char('e') => self.env_form(),
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
            KeyCode::Char('R') => self.rename_pane_form(),
            KeyCode::Char('x') => self.close_pane_confirm(),
            KeyCode::Char('X') => self.close_tab_confirm(),
            KeyCode::Char('z') => self.toggle_zoom(),
            KeyCode::Char('c') | KeyCode::PageUp => {
                let Some(id) = self.focused_pane() else {
                    self.flash_error("open a workspace first");
                    return None;
                };
                self.focus = Focus::Workspace;
                if !self.enter_copy(&id) {
                    self.flash_error("nothing to scroll in this pane yet");
                } else if k.code == KeyCode::PageUp {
                    self.copy_key(KeyEvent::from(KeyCode::PageUp));
                }
            }
            _ => {} // Esc and anything unknown cancel
        }
        None
    }

    fn workspace_key(&mut self, k: KeyEvent) -> Option<Out> {
        let id = self.focused_pane()?;
        if self.copy_of(&id).is_some() {
            self.copy_key(k);
            return None;
        }
        let slot = self.slots.get(&id)?;
        let ended = slot.view.as_ref().is_some_and(|v| v.ended.is_some());
        if (ended || slot.error.is_some() || slot.idle) && k.code == KeyCode::Enter {
            // A fresh shell where the program ended; a plain retry after a
            // failed attach, or the first start of a pane left alone.
            self.restart(&id, ended, true);
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
        let prog = command.and_then(|c| c.split_whitespace().next()).map(paths::program_name);
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

    fn rename_pane_form(&mut self) {
        let Some(pane) = self.focused_pane() else {
            self.flash_error("open a workspace first");
            return;
        };
        let name = self.doc.pane(&pane).and_then(|(_, _, p)| p.name.clone()).unwrap_or_default();
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::RenamePane { pane },
            title: "rename pane (empty: show what it runs)".into(),
            fields: vec![Self::field("name", name)],
            focus: 0,
            error: None,
        }));
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
            KeyCode::Char('e') => self.env_form(),
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

    /// Edit the environment of the cursor's project.
    fn env_form(&mut self) {
        let Some(p) = self.current_project().and_then(|id| self.doc.project(&id)) else {
            self.flash_error("add a project first");
            return;
        };
        self.dialog = Some(Dialog::Form(Form {
            kind: FormKind::Env { project: p.id.clone() },
            title: format!("env of {} (every new terminal gets it)", p.name),
            fields: vec![Field { label: "env", input: TextInput::multiline(workspace::env_text(&p.env)) }],
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
        let mut v: Vec<String> = ws.iter().flat_map(|w| w.all_panes()).filter_map(|p| p.terminal.clone()).collect();
        let ids: Vec<String> = ws.iter().map(|w| w.id.clone()).collect();
        v.extend(self.labelled(LABEL_WS, &ids));
        v.sort();
        v.dedup();
        v
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
                let max = ws_ui::help_lines("").len().saturating_sub(self.screen.height.saturating_sub(4) as usize);
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
            Some(Dialog::Form(f)) => match k.code {
                KeyCode::Esc => self.dialog = None,
                // The way to save a form whose Enter adds a line.
                KeyCode::Char('s') if k.modifiers.contains(KeyModifiers::CONTROL) => self.submit_form(),
                _ if f.fields[f.focus].input.multiline && !matches!(k.code, KeyCode::Tab | KeyCode::BackTab) => {
                    if f.fields[f.focus].input.handle(k) {
                        f.error = None;
                    }
                }
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
                let mut terminals: Vec<String> = self.terminal_of(&pane).into_iter().collect();
                terminals.extend(self.labelled(LABEL_PANE, std::slice::from_ref(&pane)));
                terminals.sort();
                terminals.dedup();
                self.close_with(terminals, WsOp::ClosePane { pane });
            }
            ConfirmKind::CloseTab(id) => {
                let (mut terminals, ids): (Vec<String>, Vec<String>) = match self.doc.tab(&id) {
                    Some((_, t)) => (
                        t.all_panes().iter().filter_map(|p| p.terminal.clone()).collect(),
                        t.all_panes().iter().map(|p| p.id.clone()).collect(),
                    ),
                    None => (vec![], vec![]),
                };
                terminals.extend(self.labelled(LABEL_PANE, &ids));
                terminals.sort();
                terminals.dedup();
                self.close_with(terminals, WsOp::CloseTab { id });
            }
            ConfirmKind::RestartEnv(panes) => self.restart_panes(&panes),
        }
    }

    /// The document now holds the environment we sent: offer to restart the
    /// project's running terminals so they pick it up.
    fn offer_restart(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        let Some((project, _)) = self.pending_env.take_if(|(id, env)| self.doc.project(id).is_some_and(|p| p.env == *env)) else {
            return;
        };
        let Some(p) = self.doc.project(&project) else { return };
        let panes = running_panes(&self.doc, &self.set, &project, self.self_addr.as_deref());
        if panes.is_empty() {
            return;
        }
        let list: Vec<String> = panes.iter().map(|(_, what)| format!("  {what}")).collect();
        let message = format!(
            "Restart {} terminal{} in {} to apply the new env?  y / n\n{}",
            panes.len(),
            if panes.len() == 1 { "" } else { "s" },
            p.name,
            list.join("\n")
        );
        let ids = panes.into_iter().map(|(id, _)| id).collect();
        self.dialog = Some(Dialog::Confirm { kind: ConfirmKind::RestartEnv(ids), message });
    }

    /// End these panes' programs and start a fresh one in each. Panes that are
    /// not on screen get theirs in the background.
    fn restart_panes(&mut self, panes: &[String]) {
        for pane in panes {
            if self.starting.contains(pane) {
                continue;
            }
            let Some((w, _, _)) = self.doc.pane(pane) else { continue };
            let ws = w.id.clone();
            let mut old: Vec<String> = self.terminal_of(pane).into_iter().collect();
            old.extend(self.labelled(LABEL_PANE, std::slice::from_ref(pane)));
            old.sort();
            old.dedup();
            let on_screen = self.open.as_deref() == Some(ws.as_str()) && self.on_screen(pane);
            self.slots.remove(pane);
            self.actions.push(Action::Detach(pane.clone()));
            if on_screen {
                self.open_slot(pane, true, true);
            } else if let Some(Plan::Start { pane, params }) = plan(&self.doc, &self.set, &ws, pane, true) {
                self.starting.insert(pane.clone());
                self.actions.push(Action::Start { pane, params });
            }
            if !old.is_empty() {
                self.actions.push(Action::Kill(old));
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
                if let (FormKind::Env { project }, [WsOp::SetProjectEnv { env, .. }]) = (&form.kind, &ops[..])
                    && self.doc.project(project).is_some_and(|p| p.env != *env)
                {
                    self.pending_env = Some((project.clone(), env.clone()));
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
        if let FormKind::Env { project } = &form.kind {
            let env = workspace::parse_env(form.fields[0].input.value())?;
            return Ok((vec![WsOp::SetProjectEnv { id: project.clone(), env }], None, None));
        }
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
            FormKind::RenamePane { pane } => {
                let name = (!name.trim().is_empty()).then(|| name.trim().to_string());
                (vec![WsOp::RenamePane { pane: pane.clone(), name }], None, None)
            }
            FormKind::NewTab { workspace } => {
                let ops = self.new_tab_ops(workspace, command(&form.fields[0].input));
                (ops.ok_or("the workspace is gone")?, None, None)
            }
            FormKind::NewSplit { pane, dir } => (self.split_ops(pane, *dir, command(&form.fields[0].input)), None, None),
            FormKind::Env { .. } => unreachable!("handled above"),
        })
    }
}

/// The panes of a project whose terminal is running now, with where they are
/// (workspace › tab › name or command). Never the terminal Hiero itself runs in.
pub fn running_panes(doc: &WsDoc, set: &TerminalSet, project: &str, self_addr: Option<&str>) -> Vec<(String, String)> {
    let Some(p) = doc.project(project) else { return vec![] };
    let mut out = vec![];
    for w in &p.workspaces {
        for t in &w.tabs {
            for pane in t.all_panes() {
                let Some(addr) = &pane.terminal else { continue };
                if self_addr == Some(addr.as_str()) || fate(set.get(addr)) != Fate::Running {
                    continue;
                }
                let what = pane.name.as_deref().or(pane.command.as_deref()).unwrap_or("shell");
                out.push((pane.id.clone(), format!("{} › {} › {what}", w.name, t.name)));
            }
        }
    }
    out
}

fn command(input: &TextInput) -> Option<String> {
    Some(input.value().trim().to_string()).filter(|c| !c.is_empty())
}

/// An existing folder as an absolute path (`~` and relative paths allowed).
fn existing_dir(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the folder can't be empty".into());
    }
    let abs = paths::canonical(&expand_home(text)).map_err(|_| format!("{text}: no such folder"))?;
    if !std::path::Path::new(&abs).is_dir() {
        return Err(format!("{text} is not a folder"));
    }
    Ok(abs)
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

    fn labelled_terminal(addr: &str, ws: &str, pane: &str) -> Terminal {
        serde_json::from_value(serde_json::json!({
            "address": addr,
            "state": "live",
            "labels": { "hiero.ws": ws, "hiero.pane": pane },
        }))
        .unwrap()
    }

    #[test]
    fn programs_whose_pane_is_gone_are_ended_after_a_while() {
        let mut a = app();
        open_attached(&mut a, "w1");
        // Its pane is gone; a hidden pane's program; one of a deleted workspace; the open pane's own.
        a.set.terminals.insert("t-orphan".into(), labelled_terminal("t-orphan", "w1", "n-gone"));
        a.set.terminals.insert("t-kept".into(), labelled_terminal("t-kept", "w-deleted", "n-x"));
        a.set.terminals.insert("t-mine".into(), labelled_terminal("t-mine", "w1", "n-w1"));
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SplitPane { pane: "n-w1".into(), dir: Dir::Horizontal, new_id: "n-hid".into() }).unwrap();
        apply(&mut doc, &WsOp::HideOthers { pane: "n-w1".into() }).unwrap();
        a.on_doc(doc);
        a.set.terminals.insert("t-hidden".into(), labelled_terminal("t-hidden", "w1", "n-hid"));
        a.actions.clear();
        let t0 = Instant::now();
        a.sweep_orphans(t0);
        assert!(!a.actions.iter().any(|x| matches!(x, Action::Kill(_))), "not at once: {:?}", a.actions);
        a.sweep_orphans(t0 + ORPHAN_AFTER);
        let kills: Vec<_> = a.actions.iter().filter_map(|x| if let Action::Kill(v) = x { Some(v.clone()) } else { None }).collect();
        assert_eq!(kills, vec![vec!["t-orphan".to_string()]]);
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

    fn type_text(a: &mut WsApp, text: &str) {
        for c in text.chars() {
            a.on_key(if c == '\n' { code(KeyCode::Enter) } else { key(c) });
        }
    }

    fn ctrl_s() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
    }

    fn env_of(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// n-w1 (p1) and n-w2 (p1) each with a terminal, in the set as live.
    fn with_running_panes(a: &mut WsApp) {
        let mut doc = a.doc.clone();
        for (pane, t) in [("n-w1", "t1"), ("n-w2", "t2"), ("n-w3", "t3")] {
            apply(&mut doc, &WsOp::SetTerminal { pane: pane.into(), terminal: Some(t.into()) }).unwrap();
            a.set.terminals.insert(t.into(), terminal(t, true));
        }
        a.on_doc(doc);
        a.actions.clear();
    }

    #[test]
    fn the_env_form_saves_lines_and_names_a_bad_one() {
        let mut a = app();
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetProjectEnv { id: "p1".into(), env: env_of(&[("OLD", "1")]) }).unwrap();
        a.on_doc(doc);

        // From a workspace row: its project's env, prefilled.
        a.sel = "w1".into();
        a.on_key(key('e'));
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert_eq!(f.fields[0].input.value(), "OLD=1");
        // Enter adds a line instead of saving.
        type_text(&mut a, "\nA=1\n# note\nB=x=y");
        assert!(a.dialog.is_some() && a.actions.is_empty());
        a.on_key(ctrl_s());
        assert!(a.dialog.is_none());
        assert_eq!(
            only_update(&mut a),
            [WsOp::SetProjectEnv { id: "p1".into(), env: env_of(&[("OLD", "1"), ("A", "1"), ("B", "x=y")]) }]
        );

        // A bad line keeps the form open and says which.
        a.sel = "p2".into();
        a.on_key(key('e'));
        type_text(&mut a, "A=1\noops");
        a.on_key(ctrl_s());
        let Some(Dialog::Form(f)) = &a.dialog else { panic!("closed") };
        assert!(f.error.as_deref().unwrap().starts_with("line 2:"));
        assert!(a.actions.is_empty());
        a.on_key(code(KeyCode::Esc));
        assert!(a.dialog.is_none());

        // The prefix key reaches it too.
        a.sel = "p2".into();
        a.on_key(ctrl_space());
        a.on_key(key('e'));
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert!(f.title.contains("solo"));
    }

    #[test]
    fn running_panes_picks_only_running_terminals_of_the_project() {
        let mut a = app();
        let mut doc = a.doc.clone();
        // A hidden pane (live), and one whose terminal the core doesn't know.
        apply(&mut doc, &WsOp::SplitPane { pane: "n-w1".into(), dir: Dir::Horizontal, new_id: "n-w1b".into() }).unwrap();
        apply(&mut doc, &WsOp::RenamePane { pane: "n-w1b".into(), name: Some("logs".into()) }).unwrap();
        for (pane, t) in [("n-w1", "t1"), ("n-w1b", "t5")] {
            apply(&mut doc, &WsOp::SetTerminal { pane: pane.into(), terminal: Some(t.into()) }).unwrap();
        }
        apply(&mut doc, &WsOp::HideOthers { pane: "n-w1".into() }).unwrap();
        apply(&mut doc, &WsOp::SplitPane { pane: "n-w1".into(), dir: Dir::Horizontal, new_id: "n-w1c".into() }).unwrap();
        for (pane, t) in [("n-w1c", "gone"), ("n-w2", "t2"), ("n-w3", "t3")] {
            apply(&mut doc, &WsOp::SetTerminal { pane: pane.into(), terminal: Some(t.into()) }).unwrap();
        }
        a.set.terminals.insert("t1".into(), terminal("t1", true));
        a.set.terminals.insert("t5".into(), terminal("t5", true));
        a.set.terminals.insert("t2".into(), finished("t2", "exited", Some(0)));
        a.set.terminals.insert("t3".into(), terminal("t3", true));

        let got = running_panes(&doc, &a.set, "p1", None);
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["n-w1", "n-w1b"], "ended, missing and other projects are left out");
        assert_eq!(got[1].1, "frontend › shell › logs");
        // Hiero's own terminal is never restarted.
        assert_eq!(running_panes(&doc, &a.set, "p1", Some("t1")).len(), 1);
        // Panes of several workspaces count.
        a.set.terminals.insert("t2".into(), terminal("t2", true));
        assert_eq!(running_panes(&doc, &a.set, "p1", None).len(), 3);
        assert_eq!(running_panes(&doc, &a.set, "nope", None), vec![]);
    }

    #[test]
    fn a_changed_env_offers_a_restart_and_yes_restarts_each_pane() {
        let mut a = app();
        with_running_panes(&mut a);
        a.sel = "p1".into();
        a.on_key(key('e'));
        type_text(&mut a, "A=1");
        a.on_key(ctrl_s());
        let ops = only_update(&mut a);
        // Nothing is asked until the document shows the new env.
        assert!(a.dialog.is_none());
        let mut doc = a.doc.clone();
        for op in &ops {
            apply(&mut doc, op).unwrap();
        }
        a.on_doc(doc);
        let Some(Dialog::Confirm { kind, message }) = &a.dialog else { panic!("no prompt") };
        assert_eq!(*kind, ConfirmKind::RestartEnv(vec!["n-w1".into(), "n-w2".into()]));
        assert!(message.starts_with("Restart 2 terminals in shosha to apply the new env?"), "{message}");
        assert!(message.contains("frontend › shell › shell") && message.contains("backend › shell › shell"), "{message}");

        // No: nothing happens.
        a.on_key(key('n'));
        assert!(a.dialog.is_none() && a.actions.is_empty());

        // Yes (asked again by hand): each pane is detached, started with the env, and its old terminal killed.
        a.dialog = Some(Dialog::Confirm { kind: ConfirmKind::RestartEnv(vec!["n-w1".into(), "n-w2".into()]), message: String::new() });
        a.on_key(key('y'));
        let starts: Vec<(&str, &str)> = a
            .actions
            .iter()
            .filter_map(|x| match x {
                Action::Start { pane, params } => Some((pane.as_str(), params.env["A"].as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [("n-w1", "1"), ("n-w2", "1")], "{:?}", a.actions);
        let kills: Vec<&Vec<String>> = a.actions.iter().filter_map(|x| if let Action::Kill(v) = x { Some(v) } else { None }).collect();
        assert_eq!(kills, [&vec!["t1".to_string()], &vec!["t2".to_string()]]);
        assert_eq!(a.actions.iter().filter(|x| matches!(x, Action::Detach(_))).count(), 2);
    }

    #[test]
    fn restarting_a_pane_on_screen_opens_a_fresh_terminal_there() {
        let mut a = app();
        with_running_panes(&mut a);
        open_attached(&mut a, "w1");
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetProjectEnv { id: "p1".into(), env: env_of(&[("A", "1")]) }).unwrap();
        a.on_doc(doc);
        a.dialog = None;
        a.restart_panes(&["n-w1".to_string()]);
        let Some(Action::Open { plan: Plan::Start { pane, params }, .. }) =
            a.actions.iter().find(|x| matches!(x, Action::Open { .. }))
        else {
            panic!("{:?}", a.actions)
        };
        assert_eq!((pane.as_str(), params.env["A"].as_str()), ("n-w1", "1"));
        assert!(a.actions.iter().any(|x| matches!(x, Action::Kill(v) if v == &["t1".to_string()])));
    }

    #[test]
    fn no_restart_is_offered_without_a_change_or_a_running_terminal() {
        let mut a = app();
        // Changed, but nothing runs.
        a.sel = "p1".into();
        a.on_key(key('e'));
        type_text(&mut a, "A=1");
        a.on_key(ctrl_s());
        let mut doc = a.doc.clone();
        for op in only_update(&mut a) {
            apply(&mut doc, &op).unwrap();
        }
        a.on_doc(doc);
        assert!(a.dialog.is_none());

        // Running, but saved as it was.
        with_running_panes(&mut a);
        a.on_key(key('e'));
        a.on_key(ctrl_s());
        a.actions.clear();
        let doc = a.doc.clone();
        a.on_doc(doc);
        assert!(a.dialog.is_none());
    }

    #[test]
    fn plan_passes_project_env_but_hiero_vars_win() {
        let mut a = app();
        let pid = a.doc.workspace("w1").unwrap().0.id.clone();
        let env = [("FOO", "bar"), (ENV_PANE, "evil"), (ENV_WS, "evil")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        apply(&mut a.doc, &WsOp::SetProjectEnv { id: pid, env }).unwrap();
        let Some(Plan::Start { params, .. }) = plan(&a.doc, &a.set, "w1", "n-w1", false) else { panic!() };
        assert_eq!(params.env["FOO"], "bar");
        assert_eq!(params.env[ENV_PANE], "n-w1");
        assert_eq!(params.env[ENV_WS], "w1");
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
        // The pane can tell it is inside Hiero.
        assert_eq!(params.env[ENV_PANE], "n-w1");
        assert_eq!(params.env[ENV_WS], "w1");

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
    fn hiero_never_attaches_to_its_own_terminal() {
        let mut a = app();
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetTerminal { pane: "n-w1".into(), terminal: Some("t9".into()) }).unwrap();
        a.doc = doc;
        a.set.terminals.insert("t9".into(), terminal("t9", true));
        a.self_addr = Some("t9".into());

        a.sel = "w1".into();
        a.on_key(code(KeyCode::Enter));
        assert_eq!(a.open.as_deref(), Some("w1"));
        assert!(!a.actions.iter().any(|x| matches!(x, Action::Open { .. })), "no attach to itself");
        let ended = a.slots["n-w1"].view.as_ref().and_then(|v| v.ended.clone());
        assert_eq!(ended.as_deref(), Some("this is Hiero itself"));

        // Another Hiero's terminal is fine to attach.
        a.self_addr = Some("t1".into());
        a.sel = "w2".into();
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        a.sel = "w1".into();
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert!(a.actions.iter().any(|x| matches!(x, Action::Open { plan: Plan::Attach { addr, .. }, .. } if addr == "t9")));
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

        // Another workspace: the running pane of the old one keeps its stream.
        a.sel = "w2".into();
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert_eq!(a.open.as_deref(), Some("w2"));
        assert!(matches!(&a.actions[..], [Action::Open { plan, .. }] if plan.pane() == "n-w2"), "{:?}", a.actions);
        assert!(a.slots.contains_key("n-w2") && a.slots["n-w1"].view.is_some());

        // Back while w2 is still opening: that one is dropped, and a late
        // attach for it is refused (so the caller closes it).
        let opening = a.slots["n-w2"].attempt;
        a.actions.clear();
        a.sel = "w1".into();
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert!(matches!(&a.actions[..], [Action::Detach(p)] if p == "n-w2"), "{:?}", a.actions);
        assert!(!a.slots.contains_key("n-w2"));
        assert!(!a.on_opened("n-w2", opening, Ok(("t2".into(), 98, 26))));
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
    fn a_pane_whose_terminal_was_cleared_in_the_document_starts_again() {
        let mut a = app();
        open_attached(&mut a, "w1");
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::SetTerminal { pane: "n-w1".into(), terminal: Some("t-n-w1".into()) }).unwrap();
        a.on_doc(doc.clone());
        a.set.terminals.insert("t-n-w1".into(), terminal("t1", true));
        a.actions.clear();
        // The pane may show no terminal at all, e.g. its open failed because the old one had ended.
        let slot = a.slots.get_mut("n-w1").unwrap();
        (slot.view, slot.error) = (None, Some("t-n-w1 has finished".into()));
        // From the CLI: a new command and no terminal.
        apply(&mut doc, &WsOp::SetCommand { pane: "n-w1".into(), command: Some("make run".into()) }).unwrap();
        apply(&mut doc, &WsOp::SetTerminal { pane: "n-w1".into(), terminal: None }).unwrap();
        a.on_doc(doc);
        let [Action::Detach(old), Action::Open { plan: Plan::Start { pane, params }, .. }] = &a.actions[..] else {
            panic!("{:?}", a.actions)
        };
        assert_eq!((old.as_str(), pane.as_str(), params.line.as_deref()), ("n-w1", "n-w1", Some("make run")));
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
        for c in std::env::temp_dir().to_string_lossy().chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        let ops = only_update(&mut a);
        let [WsOp::AddWorkspace { project, id, name, folder, .. }] = &ops[..] else { panic!("{ops:?}") };
        assert_eq!((project.as_str(), name.as_str()), ("p1", "api"));
        assert!(std::path::Path::new(folder).is_absolute() && !folder.starts_with(r"\\?\"), "{folder}");

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
    fn a_program_in_mode_2048_gets_its_size_in_band() {
        let mut a = three_panes();
        let id = a.slots.keys().min().unwrap().clone();
        a.actions.clear();
        // Turning the mode on (or seeing it in an attach's replay) reports the size once.
        a.on_stream(&id, hc_client::StreamMsg::Data(b"\x1b[?2048h".to_vec()));
        a.on_stream(&id, hc_client::StreamMsg::Data(b"\x1b[?2048h".to_vec()));
        let (cols, rows) = a.slots[&id].size;
        let reports: Vec<_> = a.actions.iter().filter(|x| matches!(x, Action::SizeReport { .. })).collect();
        assert!(matches!(&reports[..], [Action::SizeReport { pane, cols: c, rows: r }] if *pane == id && (*c, *r) == (cols, rows)), "{reports:?}");
        a.actions.clear();
        // A resize: the stream first, then the report with the new size; panes without the mode get none.
        a.set_screen(rect(0, 0, 80, 20));
        let (cols, rows) = a.slots[&id].size;
        let i = a.actions.iter().position(|x| matches!(x, Action::Resize { pane, .. } if *pane == id)).unwrap();
        assert!(matches!(a.actions.get(i + 1), Some(Action::SizeReport { pane, cols: c, rows: r }) if *pane == id && (*c, *r) == (cols, rows)), "{:?}", a.actions);
        assert_eq!(a.actions.iter().filter(|x| matches!(x, Action::SizeReport { .. })).count(), 1);
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
    fn switching_tabs_keeps_the_streams_of_running_panes() {
        let mut a = three_panes();
        let first: Vec<String> = a.active().unwrap().1.layout.panes().iter().map(|p| p.id.clone()).collect();
        // The loop records started terminals in the doc; do the same here.
        let mut doc = a.doc.clone();
        for p in &first {
            apply(&mut doc, &WsOp::SetTerminal { pane: p.clone(), terminal: Some(format!("t-{p}")) }).unwrap();
            a.set.terminals.insert(format!("t-{p}"), terminal(&format!("t-{p}"), true));
        }
        a.on_doc(doc);
        // The splits resized the panes; that resync is not the point here.
        a.resync_due(Instant::now() + Duration::from_secs(1));
        a.actions.clear();
        let sizes: Vec<(u16, u16)> = first.iter().map(|p| a.slots[p].size).collect();
        pk(&mut a, 't');
        commit(&mut a);
        // A detached terminal goes back to the core's own size, and its
        // program then prints for that size: the hidden panes stay attached.
        assert!(!a.actions.iter().any(|x| matches!(x, Action::Detach(_) | Action::Resize { .. })), "{:?}", a.actions);
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Start { .. }]), "the new tab starts a shell");
        attach_pending(&mut a);
        // Their output still reaches their screens, and asks for no redraw.
        assert!(!a.on_stream(&first[0], hc_client::StreamMsg::Data(b"while hidden".to_vec())));
        assert!(a.next_resync().is_none());

        // Back to tab 1: nothing to open, the same streams and sizes; each
        // pane takes a fresh picture from the core once.
        pk(&mut a, '1');
        assert!(opens(&mut a).is_empty());
        assert_eq!(first.iter().map(|p| a.slots[p].size).collect::<Vec<_>>(), sizes);
        assert!(a.slots[&first[0]].view.as_ref().unwrap().parser.screen().contents().contains("while hidden"));
        a.actions.clear();
        a.resync_due(a.next_resync().expect("a fresh picture is planned") + Duration::from_secs(1));
        let mut again: Vec<String> = opens(&mut a).iter().map(|p| p.pane().to_string()).collect();
        again.sort();
        let mut want = first.clone();
        want.sort();
        assert_eq!(again, want);
    }

    #[test]
    fn a_hidden_pane_that_ends_or_leaves_the_document_lets_go_of_its_stream() {
        let mut a = three_panes();
        let first: Vec<String> = a.active().unwrap().1.layout.panes().iter().map(|p| p.id.clone()).collect();
        let mut doc = a.doc.clone();
        for p in &first {
            apply(&mut doc, &WsOp::SetTerminal { pane: p.clone(), terminal: Some(format!("t-{p}")) }).unwrap();
            a.set.terminals.insert(format!("t-{p}"), terminal(&format!("t-{p}"), true));
        }
        a.on_doc(doc);
        pk(&mut a, 't');
        commit(&mut a);
        attach_pending(&mut a);

        // Its program ends while nobody looks: it starts over when shown.
        assert!(!a.on_stream(&first[0], hc_client::StreamMsg::Closed(None)));
        assert!(!a.slots.contains_key(&first[0]));
        assert!(matches!(&a.actions[..], [Action::Detach(p)] if *p == first[0]));

        // Its pane is closed from elsewhere (the CLI): the stream goes too.
        a.actions.clear();
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::ClosePane { pane: first[1].clone() }).unwrap();
        a.on_doc(doc);
        assert!(!a.slots.contains_key(&first[1]) && a.slots.contains_key(&first[2]));
        assert!(a.actions.iter().any(|x| matches!(x, Action::Detach(p) if *p == first[1])));

        // Back on the first tab the ended one opens again.
        a.actions.clear();
        pk(&mut a, '1');
        assert!(opens(&mut a).iter().any(|p| p.pane() == first[0]));
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
        // A fresh look at the tab, as when Hiero starts.
        a.slots.clear();
        a.actions.clear();
        a.sync();
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
    fn rename_pane_form() {
        let mut a = open_w1();
        let pane = a.focused_pane().unwrap();
        pk(&mut a, 'R');
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert_eq!(f.fields[0].input.value(), "", "no name yet");
        for c in "api server".chars() {
            a.on_key(key(c));
        }
        a.on_key(code(KeyCode::Enter));
        let ops = commit(&mut a);
        assert!(matches!(&ops[..], [WsOp::RenamePane { pane: p, name: Some(n) }] if *p == pane && n == "api server"));
        assert_eq!(a.doc.pane(&pane).unwrap().2.name.as_deref(), Some("api server"));

        // Opening it again shows the name; clearing it sends None.
        pk(&mut a, 'R');
        let Some(Dialog::Form(f)) = &a.dialog else { panic!() };
        assert_eq!(f.fields[0].input.value(), "api server");
        for _ in 0.."api server".len() {
            a.on_key(code(KeyCode::Backspace));
        }
        a.on_key(code(KeyCode::Enter));
        let ops = commit(&mut a);
        assert!(matches!(&ops[..], [WsOp::RenamePane { name: None, .. }]));
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

    // ---- core restart while Hiero is open ------------------------------------------

    fn snapshot(terms: Vec<Terminal>) -> WatchMsg {
        WatchMsg::Snapshot { seq: 9, terminals: terms }
    }

    fn closed(why: &str) -> hc_client::StreamMsg {
        hc_client::StreamMsg::Closed(Some(serde_json::json!({ "reason": why })))
    }

    /// two_tabs with every terminal running, the visible pane attached, and
    /// the first snapshot seen.
    fn running_two_tabs() -> (WsApp, Vec<String>) {
        let (mut a, panes) = two_tabs();
        for p in &panes {
            a.set.terminals.insert(format!("t-{p}"), terminal(&format!("t-{p}"), true));
        }
        a.connected_once = true;
        a.client_up = true;
        a.sync();
        attach_pending(&mut a);
        assert!(a.slots["n-w1"].view.is_some());
        (a, panes)
    }

    #[test]
    fn a_core_restart_brings_the_panes_back_by_itself() {
        let (mut a, _) = running_two_tabs();
        // The core stops: the stream closes, then the watcher reports it.
        a.on_stream("n-w1", closed("server_stop"));
        assert!(a.slots["n-w1"].lost && a.slots["n-w1"].view.as_ref().unwrap().ended.is_some());
        assert!(a.actions.is_empty(), "the list still says it runs: wait for the new one");
        a.on_watch(&WatchMsg::Disconnected { error: "gone".into() });
        assert!(a.disconnected.is_some() && !a.client_up);

        // The core is back and everything it had is finished by the restart;
        // the hidden x2 had been ended by the person before.
        let snap = snapshot(vec![
            finished("t-n-w1", "stopped_by_restart", None),
            finished("t-x1", "server_stop", None),
            finished("t-x2", "exited", Some(0)),
        ]);
        a.on_watch(&snap);
        assert!(a.disconnected.is_none());
        assert!(a.actions.is_empty(), "nothing starts before requests work again");
        a.sync();
        assert!(a.actions.is_empty());

        a.on_connected();
        let acts = std::mem::take(&mut a.actions);
        let mut started: Vec<String> = acts
            .iter()
            .filter_map(|x| match x {
                Action::Start { pane, .. } => Some(pane.clone()),
                Action::Open { plan: Plan::Start { pane, .. }, .. } => Some(pane.clone()),
                _ => None,
            })
            .collect();
        started.sort();
        assert_eq!(started, ["n-w1", "x1"], "x2 was ended on purpose");
        assert!(a.current_flash().is_some_and(|(m, e)| !e && m == "core restarted — restored 2 panes"), "{:?}", a.current_flash());
        assert!(a.slots["n-w1"].view.is_none() && !a.slots["n-w1"].lost, "a fresh slot, opening");

        // Asking again does nothing more.
        a.on_connected();
        a.sync();
        assert!(a.actions.is_empty());
    }

    #[test]
    fn a_pane_the_program_ended_stays_ended_after_a_reconnect() {
        let (mut a, _) = running_two_tabs();
        a.on_stream("n-w1", closed("exited"));
        a.on_watch(&WatchMsg::Disconnected { error: "gone".into() });
        a.on_watch(&snapshot(vec![
            finished("t-n-w1", "exited", Some(0)),
            terminal("t-x1", true),
            terminal("t-x2", true),
        ]));
        a.on_connected();
        assert!(a.actions.is_empty(), "{:?}", a.actions);
        assert!(a.current_flash().is_none());
        assert!(a.slots["n-w1"].view.as_ref().unwrap().ended.is_some());
        // Killed on purpose is the same.
        a.on_watch(&snapshot(vec![finished("t-n-w1", "killed", None), terminal("t-x1", true), terminal("t-x2", true)]));
        assert!(a.actions.is_empty());
    }

    #[test]
    fn a_lost_stream_to_a_running_terminal_attaches_again() {
        let (mut a, _) = running_two_tabs();
        a.on_stream("n-w1", closed("server_stop"));
        a.on_watch(&WatchMsg::Disconnected { error: "gone".into() });
        // The holder kept the terminals alive across the restart.
        a.on_watch(&snapshot(["t-n-w1", "t-x1", "t-x2"].map(|t| terminal(t, true)).to_vec()));
        a.on_connected();
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Attach { pane, addr }] if pane == "n-w1" && addr == "t-n-w1"), "{o:?}");
        assert!(a.current_flash().is_none(), "nothing was restarted");
    }

    #[test]
    fn a_pane_that_failed_to_open_while_the_core_was_away_is_tried_again() {
        let (mut a, _) = running_two_tabs();
        a.slots.get_mut("n-w1").unwrap().error = Some("not connected to herdr-core".into());
        a.on_watch(&WatchMsg::Disconnected { error: "gone".into() });
        a.on_watch(&snapshot(["t-n-w1", "t-x1", "t-x2"].map(|t| terminal(t, true)).to_vec()));
        a.on_connected();
        assert!(matches!(&opens(&mut a)[..], [Plan::Attach { pane, .. }] if pane == "n-w1"));
    }

    #[test]
    fn restore_off_leaves_a_lost_pane_idle_after_a_restart() {
        let (mut a, _) = running_two_tabs();
        a.auto_restore = false;
        a.on_stream("n-w1", closed("server_stop"));
        a.on_watch(&WatchMsg::Disconnected { error: "gone".into() });
        a.on_watch(&snapshot(vec![]));
        a.on_connected();
        assert!(!a.actions.iter().any(|x| matches!(x, Action::Start { .. } | Action::Open { .. })), "{:?}", a.actions);
        assert!(a.slots["n-w1"].idle);
    }

    #[test]
    fn a_stream_closed_by_the_system_restarts_once_the_list_says_so() {
        let (mut a, _) = running_two_tabs();
        // The terminal's own host went down while the core stayed up.
        a.set.terminals.insert("t-n-w1".into(), finished("t-n-w1", "host_shutdown", None));
        a.on_stream("n-w1", closed("host_shutdown"));
        assert!(a.actions.is_empty(), "not at once: the core may be stopping");
        a.on_tick();
        let o = opens(&mut a);
        assert!(matches!(&o[..], [Plan::Start { pane, .. }] if pane == "n-w1"), "{o:?}");
        // One that exited is left alone.
        let (mut b, _) = running_two_tabs();
        b.set.terminals.insert("t-n-w1".into(), finished("t-n-w1", "exited", Some(0)));
        b.on_stream("n-w1", closed("exited"));
        b.on_tick();
        assert!(b.actions.is_empty());
        // Not while the core is away.
        let (mut c, _) = running_two_tabs();
        c.set.terminals.insert("t-n-w1".into(), finished("t-n-w1", "server_stop", None));
        c.on_stream("n-w1", closed("server_stop"));
        c.on_watch(&WatchMsg::Disconnected { error: "gone".into() });
        c.on_tick();
        assert!(c.actions.is_empty());
    }

    // ---- sidebar: hide and resize ---------------------------------------------------

    #[test]
    fn the_sidebar_hides_and_the_panes_take_the_width() {
        let mut a = app();
        a.set_screen(rect(0, 0, 120, 40));
        open_attached(&mut a, "w1");
        a.focus = Focus::Sidebar;
        let shown = a.panes_area;
        assert_eq!(shown.width, 120 - 26);
        let size = a.slots["n-w1"].size;

        pk(&mut a, 'b');
        assert!(a.sidebar_hidden && a.sidebar_cols() == 0);
        assert_eq!(a.panes_area.width, 120);
        assert_eq!(a.focus, Focus::Workspace, "a hidden sidebar can't have the keys");
        let new = a.slots["n-w1"].size;
        assert_eq!(new.0, size.0 + 26);
        assert!(a.actions.iter().any(|x| matches!(x, Action::Resize { pane, cols, .. } if pane == "n-w1" && *cols == new.0)));
        assert_eq!(a.slots["n-w1"].view.as_ref().unwrap().parser.screen().size().1.get(), new.0, "the parser follows");

        // w asks for the sidebar: it comes back with the keyboard.
        a.actions.clear();
        pk(&mut a, 'w');
        assert!(!a.sidebar_hidden && a.focus == Focus::Sidebar);
        assert_eq!(a.slots["n-w1"].size, size);
        pk(&mut a, 'b');
        pk(&mut a, 'b');
        assert!(!a.sidebar_hidden);
    }

    /// The lines of history above a pane's screen, oldest first.
    fn history(a: &mut WsApp, pane: &str) -> Vec<String> {
        let screen = a.slots.get_mut(pane).unwrap().view.as_mut().unwrap().parser.screen_mut();
        let n = screen.scrollback_len();
        let mut out = Vec::new();
        // Page up from the oldest line, a screen at a time.
        let rows = screen.size().0.get() as usize;
        let mut top = n;
        while top > 0 {
            screen.set_scrollback(top);
            let take = rows.min(top);
            out.extend(screen.contents().lines().take(take).map(|l| l.trim_end().to_string()));
            top -= take;
        }
        screen.set_scrollback(0);
        out
    }

    #[test]
    fn panes_keep_history_and_get_it_back_from_a_fresh_attach() {
        let mut a = app();
        a.scrollback = 50;
        open_attached(&mut a, "w1");
        a.focus = Focus::Sidebar;
        let rows = a.slots["n-w1"].size.1 as usize;
        let lines: String = (1..=rows + 10).map(|i| format!("line{i}\r\n")).collect();
        feed(&mut a, "n-w1", &lines);
        let h = history(&mut a, "n-w1");
        assert_eq!(h.first().map(String::as_str), Some("line1"));
        assert_eq!(h.len(), 11, "the lines that scrolled off: {h:?}");

        // A full-screen program draws on the alternate screen: no history from it.
        feed(&mut a, "n-w1", "\x1b[?1049h");
        feed(&mut a, "n-w1", &"redraw\r\n".repeat(rows * 3));
        feed(&mut a, "n-w1", "\x1b[?1049l");
        assert_eq!(history(&mut a, "n-w1").len(), 11);

        // The cap holds.
        feed(&mut a, "n-w1", &"more\r\n".repeat(200));
        assert_eq!(history(&mut a, "n-w1").len(), 50);

        // A resync opens a new stream; the core paints history then the screen,
        // and the new parser keeps that history too.
        pk(&mut a, 'b');
        let at = a.next_resync().expect("a resync is planned");
        a.actions.clear();
        a.resync_due(at);
        let attempt = match a.actions.as_slice() {
            [Action::Open { attempt, .. }] => *attempt,
            other => panic!("{other:?}"),
        };
        let (cols, rows) = a.slots["n-w1"].size;
        assert!(a.on_opened("n-w1", attempt, Ok(("t-n-w1".into(), cols, rows))));
        let replay: String = (1..=rows as usize + 20).map(|i| format!("old{i}\r\n")).collect();
        feed(&mut a, "n-w1", &format!("\x1b[H\x1b[2J{replay}"));
        let h = history(&mut a, "n-w1");
        assert_eq!(h.first().map(String::as_str), Some("old1"));
        assert_eq!(h.len(), 21);
    }

    #[test]
    fn a_resized_pane_takes_its_screen_again_and_keeps_showing_meanwhile() {
        let mut a = app();
        open_attached(&mut a, "w1");
        a.focus = Focus::Sidebar;
        let first = a.slots["n-w1"].attempt;
        a.on_stream("n-w1", hc_client::StreamMsg::Data(b"old picture".to_vec()));
        assert!(a.next_resync().is_none());

        pk(&mut a, 'b'); // hides the sidebar: the pane grows
        let size = a.slots["n-w1"].size;
        let at = a.next_resync().expect("a resync is planned");
        a.actions.clear();
        a.resync_due(at - Duration::from_millis(1));
        assert!(a.actions.is_empty(), "not before the size has held still");
        a.resync_due(at);
        let attempt = match a.actions.as_slice() {
            [Action::Open { plan: Plan::Attach { pane, .. }, attempt, cols, rows }] if pane == "n-w1" && (*cols, *rows) == size => *attempt,
            other => panic!("{other:?}"),
        };
        assert!(attempt != first && a.next_resync().is_none());
        // Until the new stream is open the old one keeps the pane.
        let text = |a: &WsApp| a.slots["n-w1"].view.as_ref().unwrap().parser.screen().contents();
        assert_eq!((a.slots["n-w1"].attempt, text(&a).trim()), (first, "old picture"));

        // The new stream is open: its screen takes over with its first output.
        assert!(a.on_opened("n-w1", attempt, Ok(("t-n-w1".into(), size.0, size.1))));
        assert_eq!((a.slots["n-w1"].attempt, text(&a).trim()), (attempt, "old picture"), "no empty pane in between");
        a.on_stream("n-w1", hc_client::StreamMsg::Data(b"real".to_vec()));
        assert_eq!(text(&a).trim(), "real");

        // A resync that fails, or whose pane ended meanwhile, changes nothing.
        pk(&mut a, 'w');
        a.resync_due(a.next_resync().unwrap());
        let Some(Action::Open { attempt: failed, .. }) = a.actions.last() else { panic!() };
        let failed = *failed;
        a.on_stream("n-w1", hc_client::StreamMsg::Data(b"kept".to_vec()));
        assert!(!a.on_opened("n-w1", failed, Err("no".into())));
        let slot = &a.slots["n-w1"];
        assert!(slot.error.is_none() && slot.attempt == attempt && text(&a).contains("kept"));
    }

    #[test]
    fn the_sidebar_resizes_in_steps_inside_its_limits() {
        let mut a = app();
        a.set_screen(rect(0, 0, 200, 40));
        open_attached(&mut a, "w1");
        let start = a.slots["n-w1"].size.0;
        pk(&mut a, '>');
        assert_eq!(a.sidebar_w, 30);
        assert_eq!(a.slots["n-w1"].size.0, start - 4);
        pk(&mut a, '<');
        pk(&mut a, '<');
        assert_eq!(a.sidebar_w, 22);
        assert_eq!(a.slots["n-w1"].size.0, start + 4);
        for _ in 0..20 {
            pk(&mut a, '<');
        }
        assert_eq!(a.sidebar_w, ws_ui::SIDEBAR_MIN);
        for _ in 0..30 {
            pk(&mut a, '>');
        }
        assert_eq!(a.sidebar_w, ws_ui::SIDEBAR_MAX);
        // Resizing a hidden sidebar shows it.
        pk(&mut a, 'b');
        pk(&mut a, '<');
        assert!(!a.sidebar_hidden);
        // Narrow terminals give way: the workspace keeps at least half.
        a.set_screen(rect(0, 0, 50, 20));
        assert!(a.panes_area.width >= 25);
    }

    #[test]
    fn the_sidebar_state_is_remembered() {
        let mut a = app();
        a.set_sidebar(Some(38), true);
        assert_eq!((a.sidebar_w, a.sidebar_hidden), (38, true));
        a.set_sidebar(Some(500), false);
        assert_eq!(a.sidebar_w, ws_ui::SIDEBAR_MAX, "a bad saved value is clamped");
        a.set_sidebar(None, false);
        assert_eq!(a.sidebar_w, ws_ui::SIDEBAR_W);
        // It round-trips through the saved state, and old files still load.
        let state = crate::config::UiState { sidebar_width: Some(34), sidebar_hidden: true, ..Default::default() };
        let back: crate::config::UiState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!((back.sidebar_width, back.sidebar_hidden), (Some(34), true));
        let old: crate::config::UiState = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!((old.sidebar_width, old.sidebar_hidden), (None, false));
        // A hidden sidebar at start puts the keyboard in the workspace.
        let mut b = WsApp::new(Prefix::Space, None);
        b.set_sidebar(None, true);
        b.synced = true;
        let mut doc = WsDoc::default();
        add_project(&mut doc, "p1", "x");
        b.on_doc(doc);
        assert_eq!(b.focus, Focus::Workspace);
    }

    // ---- mouse ------------------------------------------------------------------

    use crossterm::event::MouseEventKind as K;

    fn ev(kind: K, x: u16, y: u16, mods: KeyModifiers) -> MouseEvent {
        MouseEvent { kind, column: x, row: y, modifiers: mods }
    }

    fn mouse(a: &mut WsApp, kind: K, x: u16, y: u16) -> Option<(String, Vec<u8>)> {
        a.on_mouse(ev(kind, x, y, KeyModifiers::NONE))
    }

    fn click(a: &mut WsApp, x: u16, y: u16) -> Option<(String, Vec<u8>)> {
        let down = mouse(a, K::Down(MouseButton::Left), x, y);
        mouse(a, K::Up(MouseButton::Left), x, y);
        down
    }

    /// Output from a pane's program (as if it came over the stream).
    fn feed(a: &mut WsApp, pane: &str, bytes: &str) {
        a.on_stream(pane, hc_client::StreamMsg::Data(bytes.as_bytes().to_vec()));
    }

    /// `n-w1` (left) | `b` (right), both attached. Screen 100x30: sidebar 0..26,
    /// panes from x=26, y=1; the border is columns 62 and 63.
    fn side_by_side() -> WsApp {
        let mut a = open_w1();
        pk(&mut a, 'v');
        commit(&mut a);
        attach_pending(&mut a);
        a.actions.clear();
        a
    }

    #[test]
    fn hit_test_finds_sidebar_rows_tabs_panes_and_borders() {
        let mut a = side_by_side();
        assert_eq!(a.panes_area, rect(26, 1, 74, 28));
        // Sidebar: border at row 0, then shosha, frontend, backend, solo, main.
        assert_eq!(a.hit_test(5, 1), Hit::Sidebar("p1".into()));
        assert_eq!(a.hit_test(5, 2), Hit::Sidebar("w1".into()));
        assert_eq!(a.hit_test(5, 5), Hit::Sidebar("w3".into()));
        assert_eq!(a.hit_test(5, 6), Hit::Nothing, "below the last row");
        assert_eq!(a.hit_test(0, 2), Hit::Nothing, "the sidebar's own border");
        // Tabs: one blank, then " 1 shell " (name of the tab), then a blank.
        let name = a.active().unwrap().1.name.clone();
        let width = format!(" 1 {name} ").chars().count() as u16;
        assert_eq!(a.hit_test(26, 0), Hit::Nothing);
        assert_eq!(a.hit_test(27, 0), Hit::Tab(0));
        assert_eq!(a.hit_test(26 + width, 0), Hit::Tab(0));
        assert_eq!(a.hit_test(26 + width + 1, 0), Hit::Nothing);
        // Panes: pane-local cell inside the border, none on it.
        assert_eq!(a.hit_test(30, 5), Hit::Pane { id: "n-w1".into(), cell: Some((3, 3)) });
        assert_eq!(a.hit_test(27, 2), Hit::Pane { id: "n-w1".into(), cell: Some((0, 0)) });
        assert_eq!(a.hit_test(26, 5), Hit::Pane { id: "n-w1".into(), cell: None });
        assert_eq!(a.hit_test(30, 1), Hit::Pane { id: "n-w1".into(), cell: None }, "the title row");
        let right = focused(&a);
        assert_eq!(a.hit_test(70, 5), Hit::Pane { id: right.clone(), cell: Some((6, 3)) });
        assert_eq!(a.hit_test(99, 5), Hit::Pane { id: right, cell: None });
        // The border is the last cell of the left pane and the first of the right.
        for x in [62, 63] {
            let Hit::Border(b) = a.hit_test(x, 10) else { panic!("{x}") };
            assert_eq!((b.index, b.dir, b.at, b.path.clone()), (0, Dir::Horizontal, 63, vec![]));
        }
        assert!(matches!(a.hit_test(61, 10), Hit::Pane { .. }) && matches!(a.hit_test(64, 10), Hit::Pane { .. }));
        // Outside the screen and the banner row.
        assert_eq!(a.hit_test(200, 5), Hit::Nothing);
        a.disconnected = Some("gone".into());
        a.set_screen(rect(0, 0, 100, 30));
        assert_eq!(a.hit_test(70, 0), Hit::Nothing, "the banner");
        assert_eq!(a.hit_test(5, 1), Hit::Nothing);
        assert_eq!(a.hit_test(5, 2), Hit::Sidebar("p1".into()), "everything moved down one row");
        assert_eq!(a.hit_test(70, 1), Hit::Nothing);
        assert_eq!(a.hit_test(27, 3), Hit::Pane { id: "n-w1".into(), cell: Some((0, 0)) });
        a.disconnected = None;
        a.set_screen(rect(0, 0, 100, 30));

        // Hidden sidebar: the panes start at the left edge.
        pk(&mut a, 'b');
        assert_eq!(a.hit_test(5, 5), Hit::Pane { id: "n-w1".into(), cell: Some((4, 3)) });
        assert!(matches!(a.hit_test(0, 5), Hit::Pane { cell: None, .. }));
        assert_eq!(a.hit_test(1, 0), Hit::Tab(0));
        let Hit::Border(b) = a.hit_test(49, 10) else { panic!() };
        assert_eq!(b.at, 50);
        pk(&mut a, 'b');

        // Zoom: only the focused pane, no borders.
        let f = focused(&a);
        pk(&mut a, 'z');
        assert_eq!(a.hit_test(63, 10), Hit::Pane { id: f, cell: Some((36, 8)) });
        assert_eq!(a.hit_test(26, 10), Hit::Pane { id: focused(&a), cell: None });
    }

    #[test]
    fn hit_test_finds_the_borders_of_nested_splits() {
        let a = three_panes();
        // n-w1 | (b over c): the vertical border, and the stacked one in the right half.
        let Hit::Border(v) = a.hit_test(62, 3) else { panic!() };
        assert_eq!((v.dir, v.path.clone()), (Dir::Horizontal, vec![]));
        let rects = a.pane_rects();
        let b = &rects[1].1;
        let y = b.y + b.height; // first row of c
        let Hit::Border(h) = a.hit_test(80, y) else { panic!() };
        assert_eq!((h.dir, h.path.clone(), h.index), (Dir::Vertical, vec![1], 0));
        let Hit::Border(h2) = a.hit_test(80, y - 1) else { panic!() };
        assert_eq!(h, h2);
        // The corner where both meet belongs to the inner split (the nearer one).
        assert!(matches!(a.hit_test(63, y), Hit::Border(Border { dir: Dir::Vertical, .. })));
        assert!(matches!(a.hit_test(63, y - 3), Hit::Border(Border { dir: Dir::Horizontal, .. })));
    }

    #[test]
    fn clicking_the_sidebar_opens_or_folds_and_moves_the_selection() {
        let mut a = app();
        a.set_screen(rect(0, 0, 100, 30));
        // frontend
        assert_eq!(click(&mut a, 5, 2), None);
        assert_eq!(a.open.as_deref(), Some("w1"));
        assert_eq!((a.sel.as_str(), a.focus), ("w1", Focus::Workspace));
        // A project folds, and the selection follows even without opening.
        a.actions.clear();
        click(&mut a, 5, 1);
        assert_eq!((a.sel.as_str(), a.focus), ("p1", Focus::Sidebar));
        assert_eq!(a.open.as_deref(), Some("w1"), "folding doesn't close the open workspace");
        let ops = only_update(&mut a);
        assert_eq!(ops, vec![WsOp::SetCollapsed { id: "p1".into(), collapsed: true }]);
        // The fold takes effect when the document comes back; rows are now
        // shosha, solo, main.
        let mut doc = a.doc.clone();
        apply(&mut doc, &ops[0]).unwrap();
        a.on_doc(doc);
        assert_eq!(ids(&a), ["p1", "p2", "w3"]);
        click(&mut a, 5, 3);
        assert_eq!((a.open.as_deref(), a.sel.as_str()), (Some("w3"), "w3"));
        // The wheel moves the cursor.
        mouse(&mut a, K::ScrollDown, 5, 4);
        assert_eq!(a.sel, "w3");
        mouse(&mut a, K::ScrollUp, 5, 4);
        assert_eq!(a.sel, "p2");
        mouse(&mut a, K::ScrollUp, 5, 4);
        assert_eq!(a.sel, "p1");
        mouse(&mut a, K::ScrollUp, 5, 4);
        assert_eq!(a.sel, "p1", "stops at the top");
        // Other buttons do nothing in the sidebar.
        let before = a.open.clone();
        mouse(&mut a, K::Down(MouseButton::Right), 5, 5);
        assert_eq!((a.open, a.sel.as_str()), (before, "p1"));
    }

    #[test]
    fn clicking_a_tab_switches_to_it() {
        let (mut a, _) = two_tabs();
        a.open_workspace("w1");
        attach_pending(&mut a);
        let cells = ws_ui::tab_cells(a.active().unwrap().0);
        assert_eq!(cells.len(), 2);
        a.focus = Focus::Sidebar;
        let x = a.screen.x + a.sidebar_cols() + cells[1].0 + 1;
        click(&mut a, x, 0);
        assert_eq!(a.focus, Focus::Workspace);
        let ops = only_update_after(&mut a);
        assert_eq!(ops, vec![WsOp::SetActiveTab { workspace: "w1".into(), index: 1 }]);
        assert_eq!(a.active().unwrap().0.active_tab, 1);
        // The tab that is already active: nothing to write.
        click(&mut a, x, 0);
        assert!(a.actions.iter().all(|x| !matches!(x, Action::Update(_))));
    }

    #[test]
    fn clicking_a_pane_focuses_it_and_sends_nothing_without_mouse_reporting() {
        let mut a = side_by_side();
        let right = focused(&a);
        a.focus = Focus::Sidebar;
        assert_eq!(click(&mut a, 30, 5), None);
        assert_eq!(a.focus, Focus::Workspace);
        assert_eq!(focused(&a), "n-w1");
        assert_eq!(only_update(&mut a), vec![WsOp::SetFocus { pane: "n-w1".into() }]);
        // Already focused: no op. A click on the border of a pane focuses it too.
        click(&mut a, 30, 5);
        assert!(a.actions.is_empty());
        click(&mut a, 99, 5);
        assert_eq!(focused(&a), right);
        assert_eq!(only_update(&mut a), vec![WsOp::SetFocus { pane: right }]);
    }

    #[test]
    fn mouse_reports_go_to_programs_that_asked_for_them() {
        let mut a = side_by_side();
        let right = focused(&a);
        // Left pane: SGR reporting. Click at screen (30, 5): pane-local cell (3, 3).
        feed(&mut a, "n-w1", "\x1b[?1000h\x1b[?1006h");
        let (pane, bytes) = mouse(&mut a, K::Down(MouseButton::Left), 30, 5).unwrap();
        assert_eq!((pane.as_str(), bytes), ("n-w1", b"\x1b[<0;4;4M".to_vec()));
        // The click also moved the focus there.
        assert_eq!(focused(&a), "n-w1");
        // The release (PressRelease mode reports it).
        let (pane, bytes) = mouse(&mut a, K::Up(MouseButton::Left), 30, 5).unwrap();
        assert_eq!((pane.as_str(), bytes), ("n-w1", b"\x1b[<0;4;4m".to_vec()));
        // The right pane doesn't report: its click focuses and sends nothing.
        assert_eq!(click(&mut a, 70, 5), None);
        assert_eq!(focused(&a), right);
        // The border and the title are not the program's.
        assert_eq!(click(&mut a, 30, 1), None);
        // Modifiers and other buttons.
        let r = a.on_mouse(ev(K::Down(MouseButton::Right), 27, 2, KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert_eq!(r, Some(("n-w1".into(), b"\x1b[<22;1;1M".to_vec())));
        a.on_mouse(ev(K::Up(MouseButton::Right), 27, 2, KeyModifiers::NONE));
    }

    #[test]
    fn mouse_encodings_follow_what_the_program_chose() {
        let mut a = side_by_side();
        feed(&mut a, "n-w1", "\x1b[?1000h");
        let (_, b) = mouse(&mut a, K::Down(MouseButton::Left), 30, 5).unwrap();
        assert_eq!(b, [0x1b, b'[', b'M', 32, 36, 36], "X10 bytes");
        let (_, b) = mouse(&mut a, K::Up(MouseButton::Left), 30, 5).unwrap();
        assert_eq!(b, [0x1b, b'[', b'M', 35, 36, 36], "a release has no button");
        feed(&mut a, "n-w1", "\x1b[?1005h");
        let (_, b) = mouse(&mut a, K::Down(MouseButton::Left), 60, 5).unwrap();
        assert_eq!(b, [&[0x1b, b'[', b'M', 32][..], "\u{42}".as_bytes(), &[36]].concat(), "UTF-8: 1-based column 34 + 32");
        mouse(&mut a, K::Up(MouseButton::Left), 60, 5);
        // X10 mode `?9`: presses only, no release.
        feed(&mut a, "n-w1", "\x1b[?1005l\x1b[?1000l\x1b[?9h");
        let (_, b) = a.on_mouse(ev(K::Down(MouseButton::Left), 30, 5, KeyModifiers::CONTROL)).unwrap();
        assert_eq!(b[3], 32, "X10 mode has no modifiers");
        assert_eq!(mouse(&mut a, K::Up(MouseButton::Left), 30, 5), None);
        // Off again: nothing goes to the program.
        feed(&mut a, "n-w1", "\x1b[?9l");
        assert_eq!(mouse(&mut a, K::Down(MouseButton::Left), 30, 5), None);
    }

    #[test]
    fn a_press_is_followed_until_release_wherever_the_pointer_goes() {
        let mut a = side_by_side();
        let right = focused(&a);
        feed(&mut a, "n-w1", "\x1b[?1002h\x1b[?1006h");
        mouse(&mut a, K::Down(MouseButton::Left), 30, 5).unwrap();
        // Dragging inside, then far into the other pane and off the screen.
        let (p, b) = mouse(&mut a, K::Drag(MouseButton::Left), 31, 5).unwrap();
        assert_eq!((p.as_str(), b), ("n-w1", b"\x1b[<32;5;4M".to_vec()));
        let (p, b) = mouse(&mut a, K::Drag(MouseButton::Left), 80, 40).unwrap();
        assert_eq!(p, "n-w1", "the right pane doesn't get it");
        // Clamped to the last cell of the left pane: its inner size is 35 x 26.
        let (w, h) = a.slots["n-w1"].size;
        assert_eq!(b, format!("\x1b[<32;{w};{h}M").into_bytes());
        let (p, b) = mouse(&mut a, K::Up(MouseButton::Left), 80, 40).unwrap();
        assert_eq!(p, "n-w1");
        assert_eq!(b, format!("\x1b[<0;{w};{h}m").into_bytes());
        // After the release a drag belongs to nobody.
        assert_eq!(mouse(&mut a, K::Drag(MouseButton::Left), 31, 5), None);
        assert_eq!(mouse(&mut a, K::Up(MouseButton::Left), 31, 5), None);
        assert_eq!(focused(&a), "n-w1");
        let _ = right;
    }

    #[test]
    fn motion_is_sent_only_in_the_modes_that_ask_for_it() {
        let mut a = side_by_side();
        // PressRelease: no drag events.
        feed(&mut a, "n-w1", "\x1b[?1000h\x1b[?1006h");
        mouse(&mut a, K::Down(MouseButton::Left), 30, 5).unwrap();
        assert_eq!(mouse(&mut a, K::Drag(MouseButton::Left), 31, 5), None);
        mouse(&mut a, K::Up(MouseButton::Left), 31, 5).unwrap();
        // ButtonMotion: drags, but not a bare move.
        feed(&mut a, "n-w1", "\x1b[?1002h");
        mouse(&mut a, K::Down(MouseButton::Left), 30, 5).unwrap();
        assert!(mouse(&mut a, K::Drag(MouseButton::Left), 31, 5).is_some());
        mouse(&mut a, K::Up(MouseButton::Left), 31, 5).unwrap();
        assert_eq!(mouse(&mut a, K::Moved, 32, 6), None);
        // AnyMotion: bare moves, in the pane under the pointer only.
        feed(&mut a, "n-w1", "\x1b[?1003h");
        let (p, b) = mouse(&mut a, K::Moved, 32, 6).unwrap();
        assert_eq!((p.as_str(), b), ("n-w1", b"\x1b[<35;6;5M".to_vec()));
        assert_eq!(mouse(&mut a, K::Moved, 70, 6), None, "the other pane has no reporting");
        assert_eq!(mouse(&mut a, K::Moved, 26, 6), None, "the border");
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line{i}\r\n")).collect()
    }

    fn clipboard(a: &mut WsApp) -> Vec<String> {
        a.actions.iter().filter_map(|x| if let Action::Clipboard(t) = x { Some(t.clone()) } else { None }).collect()
    }

    #[test]
    fn copy_mode_takes_the_keys_and_copies_to_the_clipboard() {
        let mut a = side_by_side();
        let right = focused(&a);
        feed(&mut a, &right, &numbered(60));
        pk(&mut a, 'c');
        assert!(a.copy_of(&right).is_some());
        // Keys move the cursor instead of reaching the program.
        assert_eq!(a.on_key(key('k')), None);
        assert_eq!(a.on_key(key('V')), None);
        assert_eq!(a.on_key(key('k')), None);
        assert_eq!(a.on_paste("pasted"), None, "no paste into a pane in copy mode");
        assert_eq!(a.on_key(key('y')), None);
        assert!(a.copy.is_none(), "y leaves");
        assert_eq!(clipboard(&mut a), ["line59\nline60"]);
        assert!(a.current_flash().is_some_and(|(m, e)| !e && m == "copied 2 lines"));
        // Back to the live screen: keys reach the program again.
        assert!(matches!(a.on_key(key('k')), Some(Out::Key(_))));
    }

    #[test]
    fn copy_mode_ends_when_the_pane_loses_the_focus_or_its_size() {
        let mut a = side_by_side();
        let right = focused(&a);
        feed(&mut a, &right, &numbered(60));
        let screen = |a: &WsApp| a.slots[&right].view.as_ref().unwrap().parser.screen().scrollback();
        pk(&mut a, 'c');
        a.on_key(KeyEvent::from(KeyCode::PageUp));
        assert!(screen(&a) > 0);
        pk(&mut a, 'h');
        assert!(a.copy.is_none());
        assert_eq!(screen(&a), 0, "the pane shows its live screen again");

        pk(&mut a, 'l');
        pk(&mut a, 'c');
        assert!(a.copy.is_some());
        pk(&mut a, 'H');
        commit(&mut a);
        assert!(a.copy.is_none(), "a resize renumbers the lines");

        // ctrl+b PageUp starts it a page up.
        prefix_key(&mut a, KeyEvent::from(KeyCode::PageUp));
        assert!(a.copy.is_some() && screen(&a) > 0);
        a.on_key(key('q'));
        assert!(a.copy.is_none());
    }

    #[test]
    fn the_wheel_scrolls_a_shell_back_and_leaves_at_the_bottom() {
        let mut a = side_by_side();
        let right = focused(&a);
        feed(&mut a, "n-w1", &numbered(60));
        // Over the left pane (not focused): it takes the focus and scrolls.
        assert_eq!(mouse(&mut a, K::ScrollUp, 30, 5), None);
        assert_eq!(focused(&a), "n-w1");
        assert!(a.copy_of("n-w1").is_some_and(|c| c.from_wheel));
        let up = |a: &WsApp| a.slots["n-w1"].view.as_ref().unwrap().parser.screen().scrollback();
        assert_eq!(up(&a), WHEEL_LINES);
        mouse(&mut a, K::ScrollUp, 30, 5);
        assert_eq!(up(&a), 2 * WHEEL_LINES);
        // New output doesn't pull the view down.
        feed(&mut a, "n-w1", "more\r\n");
        assert_eq!(up(&a), 2 * WHEEL_LINES + 1);
        mouse(&mut a, K::ScrollDown, 30, 5);
        mouse(&mut a, K::ScrollDown, 30, 5);
        mouse(&mut a, K::ScrollDown, 30, 5);
        assert!(a.copy.is_none(), "back at the bottom");
        assert_eq!(up(&a), 0);
        // A shell with no history yet: nothing to do.
        assert_eq!(mouse(&mut a, K::ScrollUp, 70, 5), None);
        assert!(a.copy.is_none());
        let _ = right;
    }

    #[test]
    fn dragging_in_copy_mode_selects() {
        let mut a = side_by_side();
        let right = focused(&a);
        feed(&mut a, &right, "first line\r\nsecond line\r\n");
        pk(&mut a, 'c');
        // The right pane starts at x=63 (inner x=64), y=1 (inner y=2) on the right.
        assert_eq!(mouse(&mut a, K::Down(MouseButton::Left), 64 + 6, 2), None);
        assert_eq!(mouse(&mut a, K::Drag(MouseButton::Left), 64 + 5, 3), None);
        mouse(&mut a, K::Up(MouseButton::Left), 64 + 5, 3);
        a.on_key(key('y'));
        assert_eq!(clipboard(&mut a), ["line\nsecond"]);
    }

    #[test]
    fn the_wheel_goes_to_mouse_programs_else_arrows_on_the_alternate_screen() {
        let mut a = side_by_side();
        let right = focused(&a);
        // Nothing: a plain shell has no scrollback to move yet.
        assert_eq!(mouse(&mut a, K::ScrollUp, 70, 5), None);
        // A program with reporting gets wheel events, in the pane under the pointer.
        feed(&mut a, "n-w1", "\x1b[?1000h\x1b[?1006h");
        let (p, b) = mouse(&mut a, K::ScrollUp, 30, 5).unwrap();
        assert_eq!((p.as_str(), b), ("n-w1", b"\x1b[<64;4;4M".to_vec()));
        let (_, b) = mouse(&mut a, K::ScrollDown, 30, 5).unwrap();
        assert_eq!(b, b"\x1b[<65;4;4M");
        assert_eq!(focused(&a), right, "the wheel doesn't move the focus");
        // Alternate screen without reporting: three arrows.
        feed(&mut a, &right, "\x1b[?1049h");
        let (p, b) = mouse(&mut a, K::ScrollUp, 70, 5).unwrap();
        assert_eq!((p, b), (right.clone(), b"\x1b[A\x1b[A\x1b[A".to_vec()));
        let (_, b) = mouse(&mut a, K::ScrollDown, 70, 5).unwrap();
        assert_eq!(b, b"\x1b[B\x1b[B\x1b[B");
        // Application cursor keys.
        feed(&mut a, &right, "\x1b[?1h");
        let (_, b) = mouse(&mut a, K::ScrollUp, 70, 5).unwrap();
        assert_eq!(b, b"\x1bOA\x1bOA\x1bOA");
        // Sideways does nothing there; the border neither.
        assert_eq!(mouse(&mut a, K::ScrollLeft, 70, 5), None);
        assert_eq!(mouse(&mut a, K::ScrollUp, 99, 5), None);
        // An ended program gets nothing.
        a.on_stream(&right, hc_client::StreamMsg::Closed(None));
        assert_eq!(mouse(&mut a, K::ScrollUp, 70, 5), None);
    }

    #[test]
    fn dragging_a_border_resizes_locally_and_writes_once_on_release() {
        let mut a = side_by_side();
        let right = focused(&a);
        let widths = |a: &WsApp| a.pane_rects().iter().map(|(_, r)| r.width).collect::<Vec<_>>();
        assert_eq!(widths(&a), [37, 37]);
        assert_eq!(mouse(&mut a, K::Down(MouseButton::Left), 63, 5), None);
        assert_eq!(focused(&a), right, "grabbing a border doesn't focus anything");
        mouse(&mut a, K::Drag(MouseButton::Left), 56, 5);
        assert_eq!(widths(&a), [30, 44], "the border follows the pointer at once");
        // The terminals hear about it, but the document does not, yet.
        assert!(a.actions.iter().any(|x| matches!(x, Action::Resize { pane, cols, .. } if pane == "n-w1" && *cols == 28)));
        assert!(a.actions.iter().all(|x| !matches!(x, Action::Update(_))));
        assert_eq!(a.slots["n-w1"].size.0, 28);
        mouse(&mut a, K::Drag(MouseButton::Left), 50, 9);
        mouse(&mut a, K::Drag(MouseButton::Left), 52, 9);
        assert_eq!(widths(&a), [26, 48]);
        assert!(a.actions.iter().all(|x| !matches!(x, Action::Update(_))));
        mouse(&mut a, K::Up(MouseButton::Left), 52, 9);
        let ops = only_update_after(&mut a);
        let tab = a.active().unwrap().1.id.clone();
        assert_eq!(ops, vec![WsOp::SetSizes { tab, path: vec![], sizes: vec![26, 48] }]);
        // The echo of the document keeps the sizes.
        commit(&mut a);
        assert_eq!(widths(&a), [26, 48]);
        // A plain click on the border changes nothing.
        mouse(&mut a, K::Down(MouseButton::Left), 51, 5);
        mouse(&mut a, K::Up(MouseButton::Left), 51, 5);
        assert!(a.actions.iter().all(|x| !matches!(x, Action::Update(_))));
        assert_eq!(widths(&a), [26, 48]);
    }

    /// The updates among the actions, which must be exactly one batch.
    fn only_update_after(a: &mut WsApp) -> Vec<WsOp> {
        let updates: Vec<Vec<WsOp>> =
            std::mem::take(&mut a.actions).into_iter().filter_map(|x| if let Action::Update(o) = x { Some(o) } else { None }).collect();
        assert_eq!(updates.len(), 1, "{updates:?}");
        updates.into_iter().next().unwrap()
    }

    #[test]
    fn a_border_drag_keeps_both_sides_at_five_percent_and_grabs_without_a_jump() {
        let mut a = side_by_side();
        let widths = |a: &WsApp| a.pane_rects().iter().map(|(_, r)| r.width).collect::<Vec<_>>();
        // Grab the left pane's last column (62): no jump when it starts moving.
        mouse(&mut a, K::Down(MouseButton::Left), 62, 5);
        mouse(&mut a, K::Drag(MouseButton::Left), 62, 5);
        assert_eq!(widths(&a), [37, 37]);
        mouse(&mut a, K::Drag(MouseButton::Left), 0, 5);
        assert_eq!(widths(&a), [4, 70], "5% of 74 columns, rounded up");
        mouse(&mut a, K::Drag(MouseButton::Left), 500, 5);
        assert_eq!(widths(&a), [70, 4]);
        mouse(&mut a, K::Up(MouseButton::Left), 500, 5);
        assert_eq!(only_update_after(&mut a).len(), 1);
    }

    #[test]
    fn a_nested_border_resizes_only_its_own_split() {
        let mut a = three_panes();
        let rects = a.pane_rects();
        let (b, c) = (rects[1].1, rects[2].1);
        assert_eq!(b.height + c.height, 28);
        let y = c.y;
        mouse(&mut a, K::Down(MouseButton::Left), 80, y);
        mouse(&mut a, K::Drag(MouseButton::Left), 80, 8);
        mouse(&mut a, K::Up(MouseButton::Left), 80, 8);
        let ops = only_update_after(&mut a);
        let WsOp::SetSizes { path, sizes, .. } = &ops[0] else { panic!("{ops:?}") };
        assert_eq!(path, &vec![1]);
        assert_eq!(sizes, &vec![7, 21]);
        let after = a.pane_rects();
        assert_eq!(after[0].1, rects[0].1, "the left pane is untouched");
        assert_eq!((after[1].1.height, after[2].1.height), (7, 21));
        assert_eq!(after[1].1.width, b.width);
    }

    #[test]
    fn dialogs_keep_the_mouse_out_of_the_panes() {
        let mut a = side_by_side();
        feed(&mut a, "n-w1", "\x1b[?1003h\x1b[?1006h");
        a.dialog = Some(Dialog::Help(0));
        for kind in [K::Down(MouseButton::Left), K::Up(MouseButton::Left), K::Drag(MouseButton::Left), K::Moved, K::ScrollUp] {
            assert_eq!(mouse(&mut a, kind, 30, 5), None);
        }
        assert_eq!(mouse(&mut a, K::Down(MouseButton::Left), 5, 2), None);
        assert!(matches!(a.dialog, Some(Dialog::Help(_))), "a click outside closes nothing");
        assert!(a.actions.is_empty());
        // A drag that was underway is dropped when a dialog appears.
        a.dialog = None;
        mouse(&mut a, K::Down(MouseButton::Left), 30, 5).unwrap();
        a.dialog = Some(Dialog::Help(0));
        assert_eq!(mouse(&mut a, K::Up(MouseButton::Left), 30, 5), None);
        a.dialog = None;
        assert_eq!(mouse(&mut a, K::Drag(MouseButton::Left), 31, 5), None);
    }

    #[test]
    fn the_mouse_without_a_workspace_is_harmless() {
        let mut a = app();
        a.set_screen(rect(0, 0, 100, 30));
        for kind in [K::Down(MouseButton::Left), K::Drag(MouseButton::Left), K::Up(MouseButton::Left), K::Moved, K::ScrollDown] {
            assert_eq!(mouse(&mut a, kind, 60, 10), None);
            assert_eq!(mouse(&mut a, kind, 30, 0), None);
        }
        assert!(a.actions.is_empty());
    }
}
