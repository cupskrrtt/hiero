//! State and key handling of the workspace screen: the sidebar, the prefix
//! key, dialogs, and which terminal the pane shows. No I/O here: the event
//! loop (`ws_tui`) carries out the [`Action`]s this asks for, so all of it
//! can be tested.

use crate::app::{Alert, PaneView, status_from_labels};
use crate::config::Prefix;
use crate::forms::{TextInput, expand_home};
use crate::workspace::{self, Move, WsDoc, WsOp, WsStatus, new_id};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hc_client::{EventData, RunParams, TerminalSet, WatchMsg};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const LABEL_WS: &str = "hiero.ws";
pub const LABEL_PANE: &str = "hiero.pane";
pub const LABEL_PROJECT: &str = "hiero.project";

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
}

impl Plan {
    pub fn pane(&self) -> &str {
        match self {
            Plan::Attach { pane, .. } | Plan::Start { pane, .. } => pane,
        }
    }
}

/// Work for the event loop.
#[derive(Debug)]
pub enum Action {
    LoadDoc,
    Update(Vec<WsOp>),
    /// Close the attach stream (the program keeps running).
    Detach,
    Open(Plan),
}

/// W2 shows one pane per workspace: the focused pane of the active tab.
/// With `fresh` a new terminal is started even if the old one still runs.
pub fn plan(doc: &WsDoc, set: &TerminalSet, ws_id: &str, fresh: bool) -> Option<Plan> {
    let (project, w) = doc.workspace(ws_id)?;
    let tab = w.tabs.get(w.active_tab).or(w.tabs.first())?;
    let pane = tab.layout.find(&tab.focus).or_else(|| tab.layout.panes().into_iter().next())?;
    if !fresh
        && let Some(addr) = &pane.terminal
        && set.get(addr).is_some_and(|t| t.is_live())
    {
        return Some(Plan::Attach { pane: pane.id.clone(), addr: addr.clone() });
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

// ---- dialogs ----------------------------------------------------------------

pub enum FormKind {
    AddProject,
    AddWorkspace { project: String },
    Rename { id: String },
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
    Help,
    Form(Form),
    Confirm { id: String, message: String },
}

/// What a key press means for the program in the pane.
#[derive(Debug, PartialEq)]
pub enum Out {
    Key(KeyEvent),
    /// Raw bytes (the prefix key typed twice).
    Bytes(Vec<u8>),
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
    /// The pane being opened or shown (so a late attach for another one is dropped).
    pub want: Option<String>,
    pub pane: Option<PaneView>,
    pub pane_error: Option<String>,
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
            want: None,
            pane: None,
            pane_error: None,
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
            self.close_pane();
        }
        self.fix_sel();
    }

    // ---- the pane -------------------------------------------------------------

    pub fn close_pane(&mut self) {
        self.open = None;
        self.want = None;
        self.pane = None;
        self.pane_error = None;
        self.actions.push(Action::Detach);
    }

    /// Show a workspace. Does nothing when it is already shown and its
    /// program still runs.
    pub fn open_workspace(&mut self, id: &str) {
        let shown = self.open.as_deref() == Some(id)
            && (self.want.is_some() && self.pane.as_ref().is_none_or(|p| p.ended.is_none()));
        if !shown {
            self.show(id, false);
        }
    }

    fn show(&mut self, id: &str, fresh: bool) {
        self.pane = None;
        self.pane_error = None;
        self.open = Some(id.to_string());
        self.actions.push(Action::Detach);
        match plan(&self.doc, &self.set, id, fresh) {
            Some(p) => {
                self.want = Some(p.pane().to_string());
                self.actions.push(Action::Open(p));
            }
            None => {
                self.want = None;
                self.pane_error = Some("this workspace has no pane".into());
            }
        }
    }

    pub fn wants(&self, pane: &str) -> bool {
        self.want.as_deref() == Some(pane)
    }

    pub fn pane_attached(&mut self, addr: String, cols: u16, rows: u16) {
        self.pane = Some(PaneView::new(addr.clone(), addr, cols, rows));
    }

    pub fn pane_failed(&mut self, err: String) {
        self.want = None;
        self.pane_error = Some(err);
    }

    pub fn pane_live(&self) -> bool {
        self.pane.as_ref().is_some_and(|p| p.ended.is_none())
    }

    /// (project, workspace, tab, panes in the workspace) of the open workspace.
    pub fn open_title(&self) -> Option<(String, String, String, usize)> {
        let (p, w) = self.doc.workspace(self.open.as_deref()?)?;
        let tab = w.tabs.get(w.active_tab).or(w.tabs.first()).map(|t| t.name.clone()).unwrap_or_default();
        Some((p.name.clone(), w.name.clone(), tab, w.panes().len()))
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
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            KeyCode::Char('n') => self.add_workspace_form(),
            KeyCode::Char('N') => self.add_project_form(),
            _ => {} // Esc and anything unknown cancel
        }
        None
    }

    fn workspace_key(&mut self, k: KeyEvent) -> Option<Out> {
        let ended = self.pane.as_ref().is_some_and(|p| p.ended.is_some());
        if (ended || self.pane_error.is_some()) && k.code == KeyCode::Enter {
            // A fresh shell where the program ended; a plain retry after a failed attach.
            if let Some(id) = self.open.clone() {
                self.show(&id, ended);
            }
            return None;
        }
        self.pane_live().then_some(Out::Key(k))
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
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
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

    fn delete_confirm(&mut self) {
        let message = match self.selected() {
            Some(Row::Project(id)) => {
                let Some(p) = self.doc.project(&id) else { return };
                format!(
                    "Delete project \"{}\" and its {} workspace(s)? Their terminals keep running.  y / n",
                    p.name,
                    p.workspaces.len()
                )
            }
            Some(Row::Workspace(id)) => {
                let Some((_, w)) = self.doc.workspace(&id) else { return };
                format!("Delete workspace \"{}\"? Its terminals keep running.  y / n", w.name)
            }
            None => return,
        };
        // TODO(W4): offer to kill the terminals too.
        self.dialog = Some(Dialog::Confirm { id: self.sel.clone(), message });
    }

    fn dialog_key(&mut self, k: KeyEvent) {
        match self.dialog.as_mut() {
            Some(Dialog::Help) => self.dialog = None,
            Some(Dialog::Confirm { id, .. }) => match k.code {
                KeyCode::Char('y' | 'Y') => {
                    let id = id.clone();
                    let op = if self.doc.project(&id).is_some() {
                        WsOp::DeleteProject { id }
                    } else {
                        WsOp::DeleteWorkspace { id }
                    };
                    self.dialog = None;
                    self.actions.push(Action::Update(vec![op]));
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.dialog = None,
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

    /// Check the form against the document first, so mistakes show up inside
    /// the dialog instead of after it closed.
    fn submit_form(&mut self) {
        let Some(Dialog::Form(form)) = self.dialog.take() else { return };
        let built = self.build_op(&form);
        match built {
            Ok((op, pending_sel, pending_open)) => {
                let mut scratch = self.doc.clone();
                if let Err(e) = workspace::apply(&mut scratch, &op) {
                    return self.form_error(form, e);
                }
                self.pending_sel = pending_sel;
                self.pending_open = pending_open;
                self.actions.push(Action::Update(vec![op]));
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
    fn build_op(&self, form: &Form) -> Result<(WsOp, Option<String>, Option<String>), String> {
        let name = form.fields[0].input.value().trim().to_string();
        let folder = || existing_dir(form.fields[1].input.value());
        Ok(match &form.kind {
            FormKind::AddProject => {
                let id = new_id("p");
                (WsOp::AddProject { id: id.clone(), name, root: folder()? }, Some(id), None)
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
                (op, None, Some(id))
            }
            FormKind::Rename { id } => {
                let op = if self.doc.project(id).is_some() {
                    WsOp::RenameProject { id: id.clone(), name }
                } else {
                    WsOp::RenameWorkspace { id: id.clone(), name }
                };
                (op, None, None)
            }
        })
    }
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
        assert!(matches!(a.dialog, Some(Dialog::Help)));
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
        a.pane_attached("t1".into(), 80, 24);
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
        b.pane_attached("t1".into(), 80, 24);
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
        a.pane_attached("t1".into(), 80, 24);
        assert_eq!(a.on_key(key('x')), Some(Out::Key(key('x'))));
        // The sidebar's keys are not special in the workspace.
        assert_eq!(a.on_key(key('q')), Some(Out::Key(key('q'))));
        assert!(!a.quit);
        a.pane.as_mut().unwrap().ended = Some("exited".into());
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
        let Some(Plan::Start { pane, params }) = plan(&a.doc, &a.set, "w1", false) else { panic!() };
        assert_eq!(pane, "n-w1");
        assert_eq!(params.cwd.as_deref(), Some("/tmp"));
        assert_eq!(params.line, None);
        assert_eq!(params.labels[LABEL_WS], "w1");
        assert_eq!(params.labels[LABEL_PANE], "n-w1");
        assert_eq!(params.labels[LABEL_PROJECT], "shosha");

        // Finished: still start.
        a.set.terminals.insert("t9".into(), terminal("t9", false));
        assert!(matches!(plan(&a.doc, &a.set, "w1", false), Some(Plan::Start { .. })));

        // Live: attach, unless a fresh one is asked for.
        a.set.terminals.insert("t9".into(), terminal("t9", true));
        assert!(matches!(plan(&a.doc, &a.set, "w1", false), Some(Plan::Attach { addr, .. }) if addr == "t9"));
        assert!(matches!(plan(&a.doc, &a.set, "w1", true), Some(Plan::Start { .. })));

        // A pane with a command starts it through the shell line.
        let Some(Plan::Start { params, .. }) = plan(&a.doc, &a.set, "w2", false) else { panic!() };
        assert_eq!(params.line.as_deref(), Some("nvim ."));
        assert!(plan(&a.doc, &a.set, "nope", false).is_none());
    }

    #[test]
    fn opening_switches_and_ignores_repeats() {
        let mut a = app();
        a.sel = "w1".into();
        a.on_key(code(KeyCode::Enter));
        assert_eq!(a.open.as_deref(), Some("w1"));
        assert_eq!(a.focus, Focus::Workspace);
        assert!(matches!(a.actions[..], [Action::Detach, Action::Open(Plan::Start { .. })]));
        a.actions.clear();

        // Still opening: Enter again must not start a second terminal.
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert!(a.actions.is_empty());

        a.pane_attached("t1".into(), 80, 24);
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert!(a.actions.is_empty());

        // Another workspace closes the stream first.
        a.sel = "w2".into();
        a.focus = Focus::Sidebar;
        a.on_key(code(KeyCode::Enter));
        assert_eq!(a.open.as_deref(), Some("w2"));
        assert!(a.pane.is_none());
        assert!(matches!(a.actions[..], [Action::Detach, Action::Open(_)]));
        assert!(a.wants("n-w2") && !a.wants("n-w1"), "a late attach for w1 is dropped");
    }

    #[test]
    fn enter_restarts_an_ended_pane() {
        let mut a = app();
        a.open_workspace("w1");
        a.actions.clear();
        a.pane_attached("t1".into(), 80, 24);
        a.focus = Focus::Workspace;
        a.pane.as_mut().unwrap().ended = Some("exited (code 0)".into());
        a.on_key(code(KeyCode::Enter));
        let [Action::Detach, Action::Open(Plan::Start { pane, .. })] = &a.actions[..] else { panic!("{:?}", a.actions) };
        assert_eq!(pane, "n-w1");
    }

    #[test]
    fn deleting_the_open_workspace_closes_the_pane() {
        let mut a = app();
        a.open_workspace("w1");
        a.pane_attached("t1".into(), 80, 24);
        a.actions.clear();
        let mut doc = a.doc.clone();
        apply(&mut doc, &WsOp::DeleteWorkspace { id: "w1".into() }).unwrap();
        a.on_doc(doc);
        assert!(a.open.is_none() && a.pane.is_none());
        assert!(matches!(a.actions[..], [Action::Detach]));
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
        assert!(message.contains("keep running"));
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
}
