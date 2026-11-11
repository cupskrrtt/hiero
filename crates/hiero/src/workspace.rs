//! The workspace model: projects hold named workspaces, a workspace holds
//! tabs, a tab holds a tree of split panes, and each pane is one hc terminal.
//!
//! Stored in its own herdr-core document `hiero-ws`. Every change
//! is a [`WsOp`] applied by the pure [`apply`], which keeps it easy to test;
//! [`update`] does the read-modify-write against the core.

use hc_client::{AgentState, Client, ErrorKind, TerminalSet};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU32, Ordering};

pub const DOC_NAME: &str = "hiero-ws";

/// Which way a split lays out its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// Side by side (a row): children share the width.
    Horizontal,
    /// One above the other (a column): children share the height.
    Vertical,
}

/// One terminal slot in a tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: String,
    /// A name the person gave it; `None` shows what it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What to run; `None` is the machine's shell. Kept so a missing
    /// terminal can be started again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Where to run it; `None` is the workspace folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The hc terminal last seen here (it may be gone by now).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
}

impl Pane {
    pub fn shell(id: &str) -> Pane {
        Pane { id: id.into(), name: None, command: None, cwd: None, terminal: None }
    }
}

/// A pane, or a split of several layouts. In JSON a node carries
/// `"type": "pane"` or `"type": "split"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Layout {
    Pane(Pane),
    Split {
        dir: Dir,
        children: Vec<Layout>,
        /// Relative weights, one per child (a missing or zero weight counts as 1).
        #[serde(default)]
        sizes: Vec<u16>,
    },
}

impl Layout {
    /// Every pane, left to right, top to bottom.
    pub fn panes(&self) -> Vec<&Pane> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes<'a>(&'a self, out: &mut Vec<&'a Pane>) {
        match self {
            Layout::Pane(p) => out.push(p),
            Layout::Split { children, .. } => children.iter().for_each(|c| c.collect_panes(out)),
        }
    }

    pub fn find(&self, id: &str) -> Option<&Pane> {
        match self {
            Layout::Pane(p) => (p.id == id).then_some(p),
            Layout::Split { children, .. } => children.iter().find_map(|c| c.find(id)),
        }
    }

    fn find_mut(&mut self, id: &str) -> Option<&mut Pane> {
        match self {
            Layout::Pane(p) => (p.id == id).then_some(p),
            Layout::Split { children, .. } => children.iter_mut().find_map(|c| c.find_mut(id)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub name: String,
    pub layout: Layout,
    /// Id of the pane that has the keyboard.
    #[serde(default)]
    pub focus: String,
    /// Panes taken off the screen by `hiero pane hide-others` (a debug
    /// session); their programs keep running. They come back with the saved
    /// layout, or by themselves when `hidden_by` closes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<Pane>,
    /// The pane that hid them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_by: Option<String>,
}

impl Tab {
    /// The panes on screen and the hidden ones.
    pub fn all_panes(&self) -> Vec<&Pane> {
        let mut v = self.layout.panes();
        v.extend(self.hidden.iter());
        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    /// Where new panes start.
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub active_tab: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
}

/// The whole document. Unknown keys are kept as they are.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WsDoc {
    /// In the order the user sorted them.
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl WsDoc {
    pub fn from_value(v: &Value) -> WsDoc {
        if v.is_null() {
            return WsDoc::default();
        }
        WsDoc::deserialize(v).unwrap_or_else(|e| {
            tracing::warn!("doc {DOC_NAME} unreadable, starting empty: {e}");
            WsDoc::default()
        })
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn project_by_name(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    pub fn workspace(&self, id: &str) -> Option<(&Project, &Workspace)> {
        self.projects.iter().find_map(|p| p.workspaces.iter().find(|w| w.id == id).map(|w| (p, w)))
    }

    pub fn tab(&self, id: &str) -> Option<(&Workspace, &Tab)> {
        self.all_workspaces().find_map(|w| w.tabs.iter().find(|t| t.id == id).map(|t| (w, t)))
    }

    pub fn pane(&self, id: &str) -> Option<(&Workspace, &Tab, &Pane)> {
        self.all_workspaces().find_map(|w| {
            w.tabs.iter().find_map(|t| t.layout.find(id).map(|p| (w, t, p)))
        })
    }

    fn all_workspaces(&self) -> impl Iterator<Item = &Workspace> {
        self.projects.iter().flat_map(|p| p.workspaces.iter())
    }

    /// Whether any project, workspace, tab or pane already uses this id.
    fn id_taken(&self, id: &str) -> bool {
        self.projects.iter().any(|p| {
            p.id == id
                || p.workspaces.iter().any(|w| {
                    w.id == id || w.tabs.iter().any(|t| t.id == id || t.all_panes().iter().any(|p| p.id == id))
                })
        })
    }
}

impl Project {
    pub fn workspace_by_name(&self, name: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.name == name)
    }
}

impl Workspace {
    /// Every pane in every tab.
    pub fn panes(&self) -> Vec<&Pane> {
        self.tabs.iter().flat_map(|t| t.layout.panes()).collect()
    }

    /// Every pane, hidden ones too.
    pub fn all_panes(&self) -> Vec<&Pane> {
        self.tabs.iter().flat_map(|t| t.all_panes()).collect()
    }

    /// The first pane (in any tab) with this name.
    pub fn pane_by_name(&self, name: &str) -> Option<&Pane> {
        self.panes().into_iter().find(|p| p.name.as_deref() == Some(name))
    }
}

/// Up or down in the sidebar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
}

/// One change to the document. Everything is addressed by id.
#[derive(Debug, Clone, PartialEq)]
pub enum WsOp {
    AddProject { id: String, name: String, root: String },
    RenameProject { id: String, name: String },
    DeleteProject { id: String },
    MoveProject { id: String, dir: Move },
    SetCollapsed { id: String, collapsed: bool },
    /// A new workspace starts with one tab holding one shell pane.
    AddWorkspace { project: String, id: String, name: String, folder: String, tab_id: String, pane_id: String },
    RenameWorkspace { id: String, name: String },
    #[allow(dead_code)] // the CLI has no way to change a folder yet
    SetFolder { id: String, folder: String },
    DeleteWorkspace { id: String },
    /// Moves within its project.
    MoveWorkspace { id: String, dir: Move },
    /// A new tab with one shell pane; it becomes the active tab.
    AddTab { workspace: String, id: String, name: String, pane_id: String },
    /// Refused for the last tab: a workspace always has one.
    CloseTab { id: String },
    RenameTab { id: String, name: String },
    SetActiveTab { workspace: String, index: usize },
    /// A new shell pane next to `pane`, in the same folder, and it gets the focus.
    SplitPane { pane: String, dir: Dir, new_id: String },
    /// Closing the last pane of a tab closes the tab (refused for the last tab).
    ClosePane { pane: String },
    SetFocus { pane: String },
    /// Grow (`delta` > 0) or shrink the pane's share of the nearest split that
    /// lays out along `dir`. The neighbour gives or takes the difference; weights
    /// never drop below 1, and at the limit nothing changes.
    ResizePane { tab: String, pane: String, dir: Dir, delta: i16 },
    /// Set the weights of the split at `path` (child indices from the tab's
    /// root) in one go: a border dragged with the mouse. One non-zero weight
    /// per child (the drag keeps each at 5% or more, see `drag_sizes`).
    SetSizes { tab: String, path: Vec<usize>, sizes: Vec<u16> },
    /// What the pane runs, for when its terminal has to be started again.
    SetCommand { pane: String, command: Option<String> },
    /// Name a pane; `None` or blank brings back the automatic title.
    RenamePane { pane: String, name: Option<String> },
    SetTerminal { pane: String, terminal: Option<String> },
    /// Put back a tab layout saved earlier (`hiero pane layout`). Refused when
    /// the tab has a pane the layout doesn't (one you added meanwhile). The
    /// panes keep what they have now (name, command, terminal); the
    /// arrangement and sizes come back, and so do panes that were taken out
    /// (`hiero pane hide-others`), as they were saved, if no tab has them now.
    SetLayout { tab: String, layout: Layout },
    /// Take every other pane of `pane`'s tab off the screen, into its hidden
    /// list; their programs keep running.
    HideOthers { pane: String },
    /// Put a tab's hidden panes back on the screen, beside what is there.
    Unhide { tab: String },
}

fn clean_name(what: &str, name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(format!("the {what} name can't be empty"));
    }
    Ok(n.to_string())
}

fn fresh_id(doc: &WsDoc, id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("empty id".into());
    }
    if doc.id_taken(id) {
        return Err(format!("id {id} is already used"));
    }
    Ok(())
}

fn project_ix(doc: &WsDoc, id: &str) -> Result<usize, String> {
    doc.projects.iter().position(|p| p.id == id).ok_or_else(|| format!("no project with id {id}"))
}

fn ws_ix(doc: &WsDoc, id: &str) -> Result<(usize, usize), String> {
    doc.projects
        .iter()
        .enumerate()
        .find_map(|(i, p)| p.workspaces.iter().position(|w| w.id == id).map(|j| (i, j)))
        .ok_or_else(|| format!("no workspace with id {id}"))
}

fn tab_ix(doc: &WsDoc, id: &str) -> Result<(usize, usize, usize), String> {
    doc.projects
        .iter()
        .enumerate()
        .find_map(|(i, p)| {
            p.workspaces
                .iter()
                .enumerate()
                .find_map(|(j, w)| w.tabs.iter().position(|t| t.id == id).map(|k| (i, j, k)))
        })
        .ok_or_else(|| format!("no tab with id {id}"))
}

fn pane_tab_ix(doc: &WsDoc, pane: &str) -> Result<(usize, usize, usize), String> {
    doc.projects
        .iter()
        .enumerate()
        .find_map(|(i, p)| {
            p.workspaces.iter().enumerate().find_map(|(j, w)| {
                w.tabs.iter().position(|t| t.layout.find(pane).is_some()).map(|k| (i, j, k))
            })
        })
        .ok_or_else(|| format!("no pane with id {pane}"))
}

/// Swap with the neighbour; at either end nothing happens (so a repeated
/// key press or a retried write is harmless).
fn shift<T>(v: &mut [T], i: usize, dir: Move) {
    match dir {
        Move::Up if i > 0 => v.swap(i, i - 1),
        Move::Down if i + 1 < v.len() => v.swap(i, i + 1),
        _ => {}
    }
}

fn new_tab(id: &str, name: &str, pane_id: &str) -> Tab {
    Tab {
        id: id.into(),
        name: name.into(),
        layout: Layout::Pane(Pane::shell(pane_id)),
        focus: pane_id.into(),
        hidden: vec![],
        hidden_by: None,
    }
}

pub fn apply(doc: &mut WsDoc, op: &WsOp) -> Result<(), String> {
    match op {
        WsOp::AddProject { id, name, root } => {
            fresh_id(doc, id)?;
            let name = clean_name("project", name)?;
            if doc.project_by_name(&name).is_some() {
                return Err(format!("a project named \"{name}\" already exists"));
            }
            doc.projects.push(Project {
                id: id.clone(),
                name,
                root: root.clone(),
                collapsed: false,
                workspaces: vec![],
            });
        }
        WsOp::RenameProject { id, name } => {
            let i = project_ix(doc, id)?;
            let name = clean_name("project", name)?;
            if doc.projects.iter().any(|p| p.name == name && p.id != *id) {
                return Err(format!("a project named \"{name}\" already exists"));
            }
            doc.projects[i].name = name;
        }
        WsOp::DeleteProject { id } => {
            let i = project_ix(doc, id)?;
            doc.projects.remove(i);
        }
        WsOp::MoveProject { id, dir } => {
            let i = project_ix(doc, id)?;
            shift(&mut doc.projects, i, *dir);
        }
        WsOp::SetCollapsed { id, collapsed } => {
            let i = project_ix(doc, id)?;
            doc.projects[i].collapsed = *collapsed;
        }
        WsOp::AddWorkspace { project, id, name, folder, tab_id, pane_id } => {
            let pi = project_ix(doc, project)?;
            let name = clean_name("workspace", name)?;
            if doc.projects[pi].workspace_by_name(&name).is_some() {
                return Err(format!(
                    "project \"{}\" already has a workspace named \"{name}\"",
                    doc.projects[pi].name
                ));
            }
            for (n, x) in [(1, id), (2, tab_id), (3, pane_id)] {
                fresh_id(doc, x)?;
                if [id, tab_id, pane_id].iter().take(n - 1).any(|y| *y == x) {
                    return Err(format!("id {x} is used twice"));
                }
            }
            doc.projects[pi].workspaces.push(Workspace {
                id: id.clone(),
                name,
                folder: folder.clone(),
                tabs: vec![new_tab(tab_id, "shell", pane_id)],
                active_tab: 0,
            });
        }
        WsOp::RenameWorkspace { id, name } => {
            let (pi, wi) = ws_ix(doc, id)?;
            let name = clean_name("workspace", name)?;
            let p = &mut doc.projects[pi];
            if p.workspaces.iter().any(|w| w.name == name && w.id != *id) {
                return Err(format!("project \"{}\" already has a workspace named \"{name}\"", p.name));
            }
            p.workspaces[wi].name = name;
        }
        WsOp::SetFolder { id, folder } => {
            let (pi, wi) = ws_ix(doc, id)?;
            doc.projects[pi].workspaces[wi].folder = folder.clone();
        }
        WsOp::DeleteWorkspace { id } => {
            let (pi, wi) = ws_ix(doc, id)?;
            doc.projects[pi].workspaces.remove(wi);
        }
        WsOp::MoveWorkspace { id, dir } => {
            let (pi, wi) = ws_ix(doc, id)?;
            shift(&mut doc.projects[pi].workspaces, wi, *dir);
        }
        WsOp::AddTab { workspace, id, name, pane_id } => {
            let (pi, wi) = ws_ix(doc, workspace)?;
            let name = clean_name("tab", name)?;
            if doc.projects[pi].workspaces[wi].tabs.iter().any(|t| t.name == name) {
                return Err(format!("this workspace already has a tab named \"{name}\""));
            }
            fresh_id(doc, id)?;
            fresh_id(doc, pane_id)?;
            if id == pane_id {
                return Err(format!("id {id} is used twice"));
            }
            let w = &mut doc.projects[pi].workspaces[wi];
            w.tabs.push(new_tab(id, &name, pane_id));
            w.active_tab = w.tabs.len() - 1;
        }
        WsOp::CloseTab { id } => {
            let (pi, wi, ti) = tab_ix(doc, id)?;
            remove_tab(&mut doc.projects[pi].workspaces[wi], ti)?;
        }
        WsOp::RenameTab { id, name } => {
            let (pi, wi, ti) = tab_ix(doc, id)?;
            let name = clean_name("tab", name)?;
            let w = &mut doc.projects[pi].workspaces[wi];
            if w.tabs.iter().any(|t| t.name == name && t.id != *id) {
                return Err(format!("this workspace already has a tab named \"{name}\""));
            }
            w.tabs[ti].name = name;
        }
        WsOp::SetActiveTab { workspace, index } => {
            let (pi, wi) = ws_ix(doc, workspace)?;
            let w = &mut doc.projects[pi].workspaces[wi];
            if *index >= w.tabs.len() {
                return Err(format!("tab {index} doesn't exist; the workspace has {}", w.tabs.len()));
            }
            w.active_tab = *index;
        }
        WsOp::SplitPane { pane, dir, new_id } => {
            let (pi, wi, ti) = pane_tab_ix(doc, pane)?;
            fresh_id(doc, new_id)?;
            let w = &mut doc.projects[pi].workspaces[wi];
            let tab = &mut w.tabs[ti];
            let cwd = tab.layout.find(pane).and_then(|p| p.cwd.clone());
            let new = Pane { cwd, ..Pane::shell(new_id) };
            split(&mut tab.layout, pane, *dir, new);
            tab.focus = new_id.clone();
        }
        WsOp::ClosePane { pane } => {
            let (pi, wi, ti) = pane_tab_ix(doc, pane)?;
            let w = &mut doc.projects[pi].workspaces[wi];
            // The pane that hid the others goes: they come back first.
            if w.tabs[ti].hidden_by.as_deref() == Some(pane.as_str()) {
                unhide(&mut w.tabs[ti]);
            }
            if w.tabs[ti].layout.panes().len() == 1 {
                return remove_tab(w, ti);
            }
            let tab = &mut w.tabs[ti];
            let before: Vec<String> = tab.layout.panes().iter().map(|p| p.id.clone()).collect();
            remove_pane(&mut tab.layout, pane);
            normalize(&mut tab.layout);
            if tab.focus == *pane || tab.layout.find(&tab.focus).is_none() {
                // Hand the focus to whichever pane took the closed one's place.
                let at = before.iter().position(|p| p == pane).unwrap_or(0);
                let after = tab.layout.panes();
                tab.focus = after[at.min(after.len() - 1)].id.clone();
            }
        }
        WsOp::SetLayout { tab, layout } => {
            let (pi, wi, ti) = tab_ix(doc, tab)?;
            let now: Vec<String> = doc.projects[pi].workspaces[wi].tabs[ti].layout.panes().iter().map(|p| p.id.clone()).collect();
            let want: Vec<String> = layout.panes().iter().map(|p| p.id.clone()).collect();
            if now.iter().any(|id| !want.contains(id)) {
                return Err(format!("the panes of tab {tab} changed since its layout was saved"));
            }
            if let Some(id) = want.iter().find(|id| !now.contains(id) && doc.pane(id).is_some()) {
                return Err(format!("pane {id} is in another tab now"));
            }
            let t = &mut doc.projects[pi].workspaces[wi].tabs[ti];
            let mut new = layout.clone();
            refresh_panes(&mut new, &t.layout);
            // A pane coming back from the hidden list keeps what it has there.
            for h in &t.hidden {
                replace_pane(&mut new, h);
            }
            normalize(&mut new);
            t.layout = new;
            let layout = &t.layout;
            t.hidden.retain(|h| layout.find(&h.id).is_none());
            if t.hidden.is_empty() {
                t.hidden_by = None;
            }
            if t.layout.find(&t.focus).is_none() {
                t.focus = t.layout.panes()[0].id.clone();
            }
        }
        WsOp::HideOthers { pane } => {
            let (pi, wi, ti) = pane_tab_ix(doc, pane)?;
            let t = &mut doc.projects[pi].workspaces[wi].tabs[ti];
            let keep = t.layout.find(pane).cloned().ok_or_else(|| format!("no pane {pane}"))?;
            let others: Vec<Pane> = t.layout.panes().into_iter().filter(|p| p.id != *pane).cloned().collect();
            t.hidden.extend(others);
            t.hidden_by = Some(pane.clone());
            t.layout = Layout::Pane(keep);
            t.focus = pane.clone();
        }
        WsOp::Unhide { tab } => {
            let (pi, wi, ti) = tab_ix(doc, tab)?;
            unhide(&mut doc.projects[pi].workspaces[wi].tabs[ti]);
        }
        WsOp::SetFocus { pane } => {
            let (pi, wi, ti) = pane_tab_ix(doc, pane)?;
            doc.projects[pi].workspaces[wi].tabs[ti].focus = pane.clone();
        }
        WsOp::ResizePane { tab, pane, dir, delta } => {
            let (pi, wi, ti) = tab_ix(doc, tab)?;
            let layout = &mut doc.projects[pi].workspaces[wi].tabs[ti].layout;
            if layout.find(pane).is_none() {
                return Err(format!("pane {pane} is not in tab {tab}"));
            }
            resize(layout, pane, *dir, *delta as i32);
        }
        WsOp::SetSizes { tab, path, sizes: new } => {
            let (pi, wi, ti) = tab_ix(doc, tab)?;
            let mut l = &mut doc.projects[pi].workspaces[wi].tabs[ti].layout;
            for &i in path {
                l = match l {
                    Layout::Split { children, .. } if i < children.len() => &mut children[i],
                    _ => return Err(format!("no split at {path:?} in tab {tab}")),
                };
            }
            let Layout::Split { children, sizes, .. } = l else { return Err(format!("no split at {path:?} in tab {tab}")) };
            if new.len() != children.len() {
                return Err(format!("the split has {} children, not {}", children.len(), new.len()));
            }
            if new.contains(&0) {
                return Err("a pane can't have no share of the split".into());
            }
            *sizes = new.clone();
        }
        WsOp::RenamePane { pane, name } => {
            let (pi, wi, ti) = pane_tab_ix(doc, pane)?;
            let name = name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
            if let Some(p) = doc.projects[pi].workspaces[wi].tabs[ti].layout.find_mut(pane) {
                p.name = name;
            }
        }
        WsOp::SetCommand { pane, command } => {
            let (pi, wi, ti) = pane_tab_ix(doc, pane)?;
            if let Some(p) = doc.projects[pi].workspaces[wi].tabs[ti].layout.find_mut(pane) {
                p.command = command.clone();
            }
        }
        WsOp::SetTerminal { pane, terminal } => {
            let (pi, wi, ti) = pane_tab_ix(doc, pane)?;
            if let Some(p) = doc.projects[pi].workspaces[wi].tabs[ti].layout.find_mut(pane) {
                p.terminal = terminal.clone();
            }
        }
    }
    Ok(())
}

/// A workspace never has zero tabs, so the last one can't go.
fn remove_tab(w: &mut Workspace, ti: usize) -> Result<(), String> {
    if w.tabs.len() == 1 {
        return Err(format!(
            "\"{}\" is the last tab of workspace \"{}\"; delete the workspace instead",
            w.tabs[0].name, w.name
        ));
    }
    w.tabs.remove(ti);
    if ti < w.active_tab {
        w.active_tab -= 1;
    }
    w.active_tab = w.active_tab.min(w.tabs.len() - 1);
    Ok(())
}

/// Put `new` next to the pane `id`. In a split of the same direction it
/// becomes a sibling (taking half of the pane's weight); otherwise the pane
/// is replaced by a two-way split.
fn split(l: &mut Layout, id: &str, dir: Dir, new: Pane) -> Option<Pane> {
    match l {
        Layout::Pane(p) if p.id == id => {
            let old = Layout::Pane(p.clone());
            *l = Layout::Split { dir, children: vec![old, Layout::Pane(new)], sizes: vec![1, 1] };
            None
        }
        Layout::Pane(_) => Some(new),
        Layout::Split { dir: d, children, sizes } => {
            if *d == dir && let Some(i) = children.iter().position(|c| matches!(c, Layout::Pane(p) if p.id == id)) {
                sizes.resize(children.len(), 1);
                let w = sizes[i].max(2);
                sizes[i] = w - w / 2;
                sizes.insert(i + 1, w / 2);
                children.insert(i + 1, Layout::Pane(new));
                return None;
            }
            // Hand the pane on until someone takes it (`None` means placed).
            let mut new = new;
            for c in children.iter_mut() {
                new = split(c, id, dir, new)?;
            }
            Some(new)
        }
    }
}

/// No child of a split gets less than this many percent of it.
const MIN_SHARE: u32 = 5;

/// Resize in the deepest split of `dir` above the pane. `None`: the pane is
/// not in `l`; `Some(true)`: a split took care of it (even if it was at its limit).
fn resize(l: &mut Layout, id: &str, dir: Dir, delta: i32) -> Option<bool> {
    let Layout::Split { dir: d, children, sizes } = l else {
        return matches!(l, Layout::Pane(p) if p.id == id).then_some(false);
    };
    let i = children.iter().position(|c| c.find(id).is_some())?;
    if resize(&mut children[i], id, dir, delta)? {
        return Some(true);
    }
    if *d != dir {
        return Some(false);
    }
    sizes.resize(children.len(), 1);
    shift_weight(sizes, i, delta);
    Some(true)
}

/// Move `delta` of weight to child `i` from its next sibling (the previous
/// one for the last child); a negative `delta` gives weight back. Weights are
/// first made fine-grained (sum of at least 100) so a step is a few percent.
/// Neither side goes below a small floor (always at least 1).
fn shift_weight(sizes: &mut [u16], i: usize, delta: i32) {
    if sizes.len() < 2 {
        return;
    }
    let donor = if i + 1 < sizes.len() { i + 1 } else { i - 1 };
    let mut w: Vec<i32> = sizes.iter().map(|&s| s.max(1) as i32).collect();
    let sum: i32 = w.iter().sum();
    if sum < 100 {
        let k = (100 + sum - 1) / sum;
        w.iter_mut().for_each(|x| *x *= k);
    }
    // Stop at about 5% so a pane never shrinks away to no cells.
    let floor = 5;
    let d = if delta > 0 { delta.min((w[donor] - floor).max(0)) } else { -(-delta).min((w[i] - floor).max(0)) };
    if d == 0 {
        return;
    }
    w[i] += d;
    w[donor] -= d;
    for (s, x) in sizes.iter_mut().zip(w) {
        *s = x.clamp(1, u16::MAX as i32) as u16;
    }
}

fn remove_pane(l: &mut Layout, id: &str) -> bool {
    let Layout::Split { children, sizes, .. } = l else { return false };
    sizes.resize(children.len(), 1);
    if let Some(i) = children.iter().position(|c| matches!(c, Layout::Pane(p) if p.id == id)) {
        children.remove(i);
        sizes.remove(i);
        return true;
    }
    children.iter_mut().any(|c| remove_pane(c, id))
}

/// Keep the tree tidy: a split with one child becomes that child, a child
/// split in the parent's own direction is merged into the parent, and
/// `sizes` has one weight per child.
fn normalize(l: &mut Layout) {
    let Layout::Split { dir, children, sizes } = l else { return };
    sizes.resize(children.len(), 1);
    for c in children.iter_mut() {
        normalize(c);
    }
    let mut kids = Vec::new();
    let mut weights = Vec::new();
    for (c, w) in std::mem::take(children).into_iter().zip(std::mem::take(sizes)) {
        match c {
            Layout::Split { dir: d, children: cc, sizes: cs } if d == *dir => {
                let total: u32 = cs.iter().map(|&s| s.max(1) as u32).sum();
                for (g, s) in cc.into_iter().zip(cs) {
                    kids.push(g);
                    // The child's weight, shared out by its own proportions.
                    let part = (w.max(1) as u32 * s.max(1) as u32).div_ceil(total);
                    weights.push(part.clamp(1, u16::MAX as u32) as u16);
                }
            }
            c => {
                kids.push(c);
                weights.push(w);
            }
        }
    }
    *children = kids;
    *sizes = weights;
    if children.len() == 1 {
        *l = children.pop().unwrap();
    }
}

/// Cut `total` cells into parts in proportion to `weights`. The parts add
/// up to `total` exactly: part k ends where `total * (weights so far) / sum` says.
fn share(total: u16, weights: &[u16], n: usize) -> Vec<u16> {
    let w: Vec<u64> = (0..n).map(|i| weights.get(i).copied().unwrap_or(1).max(1) as u64).collect();
    let sum: u64 = w.iter().sum();
    let (mut acc, mut prev) = (0u64, 0u64);
    w.iter()
        .map(|x| {
            acc += x;
            let end = total as u64 * acc / sum;
            let len = end - prev;
            prev = end;
            len as u16
        })
        .collect()
}

/// Where each pane goes inside `area`. Every cell is given to exactly one pane.
pub fn rects(layout: &Layout, area: Rect) -> Vec<(String, Rect)> {
    let mut out = Vec::new();
    place(layout, area, &mut out);
    out
}

/// The line between two neighbours of a split, for dragging it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Border {
    /// Child indices from the root to the split.
    pub path: Vec<usize>,
    /// Between child `index` and `index + 1`.
    pub index: usize,
    pub dir: Dir,
    /// Where the split is drawn.
    pub area: Rect,
    /// Where child `index + 1` starts: column (side by side) or row (stacked).
    /// The border is the cell before it and this one.
    pub at: u16,
}

impl Border {
    /// Whether the cell is on the border: the last cell of the first pane or
    /// the first of the next, along the split's whole length.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        let a = self.area;
        if x < a.x || y < a.y || x >= a.x + a.width || y >= a.y + a.height {
            return false;
        }
        let v = if self.dir == Dir::Horizontal { x } else { y };
        v + 1 == self.at || v == self.at
    }
}

/// Every border of the layout inside `area`, outer splits first.
pub fn borders(layout: &Layout, area: Rect) -> Vec<Border> {
    let mut out = Vec::new();
    place_borders(layout, area, &mut Vec::new(), &mut out);
    out
}

fn place_borders(l: &Layout, area: Rect, path: &mut Vec<usize>, out: &mut Vec<Border>) {
    let Layout::Split { dir, children, sizes } = l else { return };
    let horizontal = *dir == Dir::Horizontal;
    let total = if horizontal { area.width } else { area.height };
    let mut at = if horizontal { area.x } else { area.y };
    let lens = share(total, sizes, children.len());
    for (i, (c, len)) in children.iter().zip(&lens).enumerate() {
        let r = if horizontal { Rect::new(at, area.y, *len, area.height) } else { Rect::new(area.x, at, area.width, *len) };
        at += len;
        if i + 1 < children.len() {
            out.push(Border { path: path.clone(), index: i, dir: *dir, area, at });
        }
        path.push(i);
        place_borders(c, r, path, out);
        path.pop();
    }
}

/// The weights of the split at `b` once its border is dragged to `pointer`
/// (a column or row): the two neighbours share their cells anew, the others
/// keep theirs. Each of the two keeps at least 5% of the split. Weights are
/// the cell counts, so what is drawn is what is saved.
pub fn drag_sizes(layout: &Layout, b: &Border, pointer: u16) -> Option<Vec<u16>> {
    let mut l = layout;
    for &i in &b.path {
        let Layout::Split { children, .. } = l else { return None };
        l = children.get(i)?;
    }
    let Layout::Split { children, sizes, .. } = l else { return None };
    let horizontal = b.dir == Dir::Horizontal;
    let (start, total) = if horizontal { (b.area.x, b.area.width) } else { (b.area.y, b.area.height) };
    let mut lens = share(total, sizes, children.len());
    if b.index + 1 >= lens.len() {
        return None;
    }
    let first: u16 = lens[..b.index].iter().sum();
    let pair = lens[b.index] + lens[b.index + 1];
    // The border sits at the start of the next child; at least 5% each side.
    let floor = ((total as u32 * MIN_SHARE).div_ceil(100) as u16).max(1);
    if pair < floor * 2 {
        return None;
    }
    let new = pointer.saturating_sub(start + first).clamp(floor, pair - floor);
    lens[b.index] = new;
    lens[b.index + 1] = pair - new;
    Some(lens)
}

fn place(l: &Layout, area: Rect, out: &mut Vec<(String, Rect)>) {
    match l {
        Layout::Pane(p) => out.push((p.id.clone(), area)),
        Layout::Split { dir, children, sizes } => {
            let horizontal = *dir == Dir::Horizontal;
            let total = if horizontal { area.width } else { area.height };
            let mut at = if horizontal { area.x } else { area.y };
            for (c, len) in children.iter().zip(share(total, sizes, children.len())) {
                let r = if horizontal {
                    Rect::new(at, area.y, len, area.height)
                } else {
                    Rect::new(area.x, at, area.width, len)
                };
                at += len;
                place(c, r, out);
            }
        }
    }
}

/// What the sidebar shows for a workspace: the most pressing state among its
/// terminals. Later variants win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WsStatus {
    /// No pane has a terminal (or none that reports).
    None,
    Idle,
    Done,
    Working,
    /// Blocked or failed: needs you.
    Attention,
}

pub fn pane_status(p: &Pane, set: &TerminalSet) -> WsStatus {
    let Some(t) = p.terminal.as_ref().and_then(|a| set.terminals.get(a)) else {
        return WsStatus::None;
    };
    match t.agent_state() {
        AgentState::Blocked | AgentState::Error => WsStatus::Attention,
        AgentState::Working => WsStatus::Working,
        AgentState::Done => WsStatus::Done,
        AgentState::Idle => WsStatus::Idle,
        AgentState::Unknown => WsStatus::None,
    }
}

pub fn workspace_status(w: &Workspace, set: &TerminalSet) -> WsStatus {
    w.panes().into_iter().map(|p| pane_status(p, set)).max().unwrap_or(WsStatus::None)
}

/// A project shows the worst of its workspaces (for when it is collapsed).
pub fn project_status(p: &Project, set: &TerminalSet) -> WsStatus {
    p.workspaces.iter().map(|w| workspace_status(w, set)).max().unwrap_or(WsStatus::None)
}

/// Read-modify-write with retries when another writer got there first. If an
/// op is refused nothing is written.
pub async fn update(c: &Client, ops: &[WsOp]) -> Result<WsDoc, String> {
    // Several writers at once is normal (a debug session opens many panes
    // while the UI records their terminals): retry, spread out a little.
    for attempt in 0..30u64 {
        if attempt > 0 {
            let jitter = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as u64 % 20);
            tokio::time::sleep(std::time::Duration::from_millis(5 + attempt.min(10) * 5 + jitter)).await;
        }
        let cur = c.doc_get(DOC_NAME).await.map_err(|e| e.to_string())?;
        let mut doc = WsDoc::from_value(&cur.value);
        for op in ops {
            apply(&mut doc, op)?;
        }
        let value = serde_json::to_value(&doc).map_err(|e| e.to_string())?;
        match c.doc_set(DOC_NAME, &value, cur.version).await {
            Ok(_) => return Ok(doc),
            Err(e) if e.is_kind(ErrorKind::Conflict) => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("the workspace list kept changing underneath; try again".into())
}

pub async fn load(c: &Client) -> Result<WsDoc, String> {
    let cur = c.doc_get(DOC_NAME).await.map_err(|e| e.to_string())?;
    Ok(WsDoc::from_value(&cur.value))
}

/// A short unique id such as `w-1a2b3c4d`. The caller makes ids so that
/// [`apply`] stays deterministic.
pub fn new_id(prefix: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs() << 20)
        .unwrap_or(0) as u32;
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:08x}", nanos.wrapping_mul(2_654_435_761).wrapping_add(n.wrapping_mul(0x9e37_79b1)))
}

/// What `hiero pane split` asks for.
#[derive(Debug, Clone)]
pub struct SplitRequest {
    pub dir: Dir,
    /// The new pane's share (1..=90 percent) of the split it lands in.
    pub size: Option<u16>,
    pub name: Option<String>,
    /// What to run (a shell line); `None` is a shell.
    pub command: Option<String>,
    /// Leave the focus where it was.
    pub no_focus: bool,
    /// With a name: reuse the pane of that name in the workspace instead of splitting.
    pub reuse: bool,
}

/// The ops for `hiero pane split` next to `target`, the id of the pane they
/// leave running the command, and the terminal of a reused pane that must
/// be ended. Nothing is applied; the ops are checked against a copy.
pub fn plan_split(doc: &WsDoc, target: &str, new_id: &str, r: &SplitRequest) -> Result<(Vec<WsOp>, String, Option<String>), String> {
    let (ws, tab, _) = doc.pane(target).ok_or_else(|| format!("no pane {target}"))?;
    let name = match r.name.as_deref().map(str::trim) {
        Some("") => return Err("the name can't be empty".into()),
        n => n,
    };
    if r.reuse && name.is_none() {
        return Err("--reuse needs --name".into());
    }
    if r.size.is_some_and(|s| !(1..=90).contains(&s)) {
        return Err("--size must be between 1 and 90".into());
    }
    let set_command = |pane: &str| WsOp::SetCommand { pane: pane.to_string(), command: r.command.clone() };
    if let Some(old) = name.filter(|_| r.reuse).and_then(|n| ws.pane_by_name(n)) {
        // The restore rule starts the pane again once it has no terminal.
        let ops = vec![set_command(&old.id), WsOp::SetTerminal { pane: old.id.clone(), terminal: None }];
        return Ok((ops, old.id.clone(), old.terminal.clone()));
    }
    let mut ops = vec![WsOp::SplitPane { pane: target.to_string(), dir: r.dir, new_id: new_id.to_string() }];
    if r.command.is_some() {
        ops.push(set_command(new_id));
    }
    if let Some(n) = name {
        ops.push(WsOp::RenamePane { pane: new_id.to_string(), name: Some(n.to_string()) });
    }
    if let Some(pct) = r.size {
        let mut scratch = doc.clone();
        apply(&mut scratch, &ops[0])?;
        let (_, t, _) = scratch.pane(new_id).ok_or("the split lost the new pane")?;
        if let Some((path, i, sizes)) = parent_split(&t.layout, new_id, &mut vec![]) {
            ops.push(WsOp::SetSizes { tab: t.id.clone(), path, sizes: share_for(&sizes, i, pct) });
        }
    }
    if r.no_focus && !tab.focus.is_empty() {
        ops.push(WsOp::SetFocus { pane: tab.focus.clone() });
    }
    let mut scratch = doc.clone();
    ops.iter().try_for_each(|op| apply(&mut scratch, op))?;
    Ok((ops, new_id.to_string(), None))
}

/// The split directly above pane `id`: its path from the root, the pane's
/// index in it, and the weights (one per child, a missing or zero one as 1).
fn parent_split(l: &Layout, id: &str, path: &mut Vec<usize>) -> Option<(Vec<usize>, usize, Vec<u16>)> {
    let Layout::Split { children, sizes, .. } = l else { return None };
    if let Some(i) = children.iter().position(|c| matches!(c, Layout::Pane(p) if p.id == id)) {
        let w = (0..children.len()).map(|k| sizes.get(k).copied().filter(|&x| x > 0).unwrap_or(1)).collect();
        return Some((path.clone(), i, w));
    }
    for (k, c) in children.iter().enumerate() {
        path.push(k);
        if let Some(found) = parent_split(c, id, path) {
            return Some(found);
        }
        path.pop();
    }
    None
}

/// Weights where child `i` gets `pct` percent and the others keep their proportions.
fn share_for(weights: &[u16], i: usize, pct: u16) -> Vec<u16> {
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let others: u64 = weights.iter().enumerate().filter(|&(k, _)| k != i).map(|(_, &w)| w as u64).sum();
    let pct = pct as u64;
    // The others scale by (100 - pct), the new one is `others * pct`.
    let mut v: Vec<u64> =
        weights.iter().enumerate().map(|(k, &w)| if k == i { others * pct } else { w as u64 * (100 - pct) }).collect();
    let g = v.iter().fold(0, |g, &x| gcd(g, x)).max(1);
    v.iter_mut().for_each(|x| *x /= g);
    let big = v.iter().copied().max().unwrap_or(1);
    let cap = u16::MAX as u64;
    v.into_iter().map(|x| (if big > cap { x * cap / big } else { x }).max(1) as u16).collect()
}

/// `l` without pane `id` (a split left with one child gives way to it).
pub fn without_pane(l: &Layout, id: &str) -> Layout {
    let mut l = l.clone();
    if l.panes().len() > 1 {
        remove_pane(&mut l, id);
        normalize(&mut l);
    }
    l
}

/// Put `p` in place of the pane with its id in `l`, if there is one.
fn replace_pane(l: &mut Layout, p: &Pane) {
    match l {
        Layout::Pane(q) if q.id == p.id => *q = p.clone(),
        Layout::Pane(_) => {}
        Layout::Split { children, .. } => children.iter_mut().for_each(|c| replace_pane(c, p)),
    }
}

/// Bring a tab's hidden panes back, beside its layout: across the direction
/// of its top split (one of the same direction would merge and lose its
/// sizes), the layout keeping the larger share.
fn unhide(t: &mut Tab) {
    t.hidden_by = None;
    if t.hidden.is_empty() {
        return;
    }
    let back: Vec<Layout> = std::mem::take(&mut t.hidden).into_iter().map(Layout::Pane).collect();
    let dir = match &t.layout {
        Layout::Split { dir: Dir::Horizontal, .. } => Dir::Vertical,
        _ => Dir::Horizontal,
    };
    let mut sizes = vec![3 * back.len() as u16];
    sizes.extend(std::iter::repeat_n(1, back.len()));
    let mut children = vec![std::mem::replace(&mut t.layout, Layout::Pane(Pane::shell("")))];
    children.extend(back);
    t.layout = Layout::Split { dir, children, sizes };
    normalize(&mut t.layout);
}

/// Replace every pane in `l` with the pane of the same id in `from`.
fn refresh_panes(l: &mut Layout, from: &Layout) {
    match l {
        Layout::Pane(p) => {
            if let Some(now) = from.find(&p.id) {
                *p = now.clone();
            }
        }
        Layout::Split { children, .. } => children.iter_mut().for_each(|c| refresh_panes(c, from)),
    }
}

/// The tree as text, for `hiero ws list`.
pub fn render_tree(doc: &WsDoc) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for p in &doc.projects {
        let mark = if p.collapsed { " (collapsed)" } else { "" };
        let _ = writeln!(s, "{}  {}{mark}", p.name, p.root);
        for w in &p.workspaces {
            let _ = writeln!(s, "  {}  {}", w.name, w.folder);
            for (i, t) in w.tabs.iter().enumerate() {
                let active = if i == w.active_tab { " *" } else { "" };
                let _ = writeln!(s, "    tab {}{active}", t.name);
                render_layout(&t.layout, &t.focus, 3, &mut s);
            }
        }
    }
    if s.is_empty() {
        s.push_str("no projects yet (hiero ws add-project <name>)\n");
    }
    s
}

fn render_layout(l: &Layout, focus: &str, depth: usize, s: &mut String) {
    use std::fmt::Write;
    let pad = "  ".repeat(depth);
    match l {
        Layout::Pane(p) => {
            let cmd = p.command.as_deref().unwrap_or("shell");
            let term = p.terminal.as_deref().unwrap_or("-");
            let f = if p.id == focus { " *" } else { "" };
            let name = p.name.as_deref().map(|n| format!("\"{n}\"  ")).unwrap_or_default();
            let _ = writeln!(s, "{pad}pane {}  {name}{cmd}  terminal {term}{f}", p.id);
        }
        Layout::Split { dir, children, sizes } => {
            let name = if *dir == Dir::Horizontal { "horizontal" } else { "vertical" };
            let w: Vec<String> = sizes.iter().map(|x| x.to_string()).collect();
            let _ = writeln!(s, "{pad}split {name} [{}]", w.join(":"));
            for c in children {
                render_layout(c, focus, depth + 1, s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(doc: &mut WsDoc, op: WsOp) {
        apply(doc, &op).unwrap_or_else(|e| panic!("{op:?}: {e}"));
    }

    fn s(x: &str) -> String {
        x.to_string()
    }

    fn add_project(doc: &mut WsDoc, id: &str, name: &str) {
        ok(doc, WsOp::AddProject { id: s(id), name: s(name), root: s("/r") });
    }

    /// Workspace `id` with tab `<id>-t` and pane `<id>-p`.
    fn add_ws(doc: &mut WsDoc, project: &str, id: &str, name: &str) {
        ok(
            doc,
            WsOp::AddWorkspace {
                project: s(project),
                id: s(id),
                name: s(name),
                folder: s("/r/f"),
                tab_id: format!("{id}-t"),
                pane_id: format!("{id}-p"),
            },
        );
    }

    fn base() -> WsDoc {
        let mut d = WsDoc::default();
        add_project(&mut d, "p1", "shosha");
        add_ws(&mut d, "p1", "w1", "frontend");
        d
    }

    fn split_op(pane: &str, dir: Dir, new_id: &str) -> WsOp {
        WsOp::SplitPane { pane: s(pane), dir, new_id: s(new_id) }
    }

    fn pane_ids(d: &WsDoc, tab: &str) -> Vec<String> {
        d.tab(tab).unwrap().1.layout.panes().iter().map(|p| p.id.clone()).collect()
    }

    fn err(d: &mut WsDoc, op: WsOp) -> String {
        let before = d.clone();
        let e = apply(d, &op).expect_err("should fail");
        assert_eq!(*d, before, "a refused op changes nothing");
        e
    }

    fn names(d: &WsDoc) -> Vec<&str> {
        d.projects.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn new_workspace_has_one_tab_with_one_shell() {
        let d = base();
        let (p, w) = d.workspace("w1").unwrap();
        assert_eq!(p.name, "shosha");
        assert_eq!(w.folder, "/r/f");
        assert_eq!(w.tabs.len(), 1);
        assert_eq!(w.tabs[0].name, "shell");
        assert_eq!(w.tabs[0].focus, "w1-p");
        assert_eq!(w.tabs[0].layout, Layout::Pane(Pane::shell("w1-p")));
        assert_eq!(w.active_tab, 0);
    }

    #[test]
    fn project_ops() {
        let mut d = WsDoc::default();
        add_project(&mut d, "a", "one");
        add_project(&mut d, "b", "two");
        add_project(&mut d, "c", "three");
        ok(&mut d, WsOp::RenameProject { id: s("b"), name: s("  deux ") });
        assert_eq!(names(&d), ["one", "deux", "three"]);
        ok(&mut d, WsOp::RenameProject { id: s("b"), name: s("deux") });
        ok(&mut d, WsOp::SetCollapsed { id: s("a"), collapsed: true });
        assert!(d.project("a").unwrap().collapsed);
        ok(&mut d, WsOp::MoveProject { id: s("c"), dir: Move::Up });
        assert_eq!(names(&d), ["one", "three", "deux"]);
        ok(&mut d, WsOp::MoveProject { id: s("a"), dir: Move::Down });
        assert_eq!(names(&d), ["three", "one", "deux"]);
        ok(&mut d, WsOp::DeleteProject { id: s("b") });
        assert_eq!(names(&d), ["three", "one"]);
    }

    #[test]
    fn project_moves_at_the_ends_do_nothing() {
        let mut d = WsDoc::default();
        add_project(&mut d, "a", "one");
        add_project(&mut d, "b", "two");
        ok(&mut d, WsOp::MoveProject { id: s("a"), dir: Move::Up });
        ok(&mut d, WsOp::MoveProject { id: s("b"), dir: Move::Down });
        assert_eq!(names(&d), ["one", "two"]);
        // A single project can't move either way.
        let mut d = WsDoc::default();
        add_project(&mut d, "a", "one");
        ok(&mut d, WsOp::MoveProject { id: s("a"), dir: Move::Up });
        ok(&mut d, WsOp::MoveProject { id: s("a"), dir: Move::Down });
    }

    #[test]
    fn project_validation() {
        let mut d = base();
        assert!(err(&mut d, WsOp::AddProject { id: s("x"), name: s("   "), root: s("/") }).contains("empty"));
        assert!(
            err(&mut d, WsOp::AddProject { id: s("x"), name: s(" shosha "), root: s("/") })
                .contains("already exists")
        );
        assert!(err(&mut d, WsOp::AddProject { id: s("p1"), name: s("new"), root: s("/") }).contains("already used"));
        assert!(err(&mut d, WsOp::AddProject { id: s("w1-p"), name: s("new"), root: s("/") }).contains("already used"));
        add_project(&mut d, "p2", "hiero");
        assert!(err(&mut d, WsOp::RenameProject { id: s("p2"), name: s("shosha") }).contains("already exists"));
        assert!(err(&mut d, WsOp::RenameProject { id: s("p2"), name: s("") }).contains("empty"));
        assert!(err(&mut d, WsOp::RenameProject { id: s("zz"), name: s("q") }).contains("no project"));
        assert!(err(&mut d, WsOp::DeleteProject { id: s("zz") }).contains("no project"));
        assert!(err(&mut d, WsOp::SetCollapsed { id: s("zz"), collapsed: true }).contains("no project"));
    }

    #[test]
    fn deleting_a_project_removes_everything_under_it() {
        let mut d = base();
        ok(&mut d, WsOp::DeleteProject { id: s("p1") });
        assert!(d.workspace("w1").is_none() && d.pane("w1-p").is_none() && d.tab("w1-t").is_none());
        // The ids are free again.
        add_project(&mut d, "p1", "shosha");
    }

    #[test]
    fn workspace_ops_and_order() {
        let mut d = base();
        add_ws(&mut d, "p1", "w2", "backend");
        add_ws(&mut d, "p1", "w3", "runner");
        let order = |d: &WsDoc| -> Vec<String> {
            d.project("p1").unwrap().workspaces.iter().map(|w| w.name.clone()).collect()
        };
        assert_eq!(order(&d), ["frontend", "backend", "runner"]);
        ok(&mut d, WsOp::MoveWorkspace { id: s("w3"), dir: Move::Up });
        assert_eq!(order(&d), ["frontend", "runner", "backend"]);
        ok(&mut d, WsOp::MoveWorkspace { id: s("w1"), dir: Move::Up });
        ok(&mut d, WsOp::MoveWorkspace { id: s("w2"), dir: Move::Down });
        assert_eq!(order(&d), ["frontend", "runner", "backend"]);
        ok(&mut d, WsOp::RenameWorkspace { id: s("w2"), name: s(" api ") });
        ok(&mut d, WsOp::SetFolder { id: s("w2"), folder: s("/srv") });
        assert_eq!(d.workspace("w2").unwrap().1.name, "api");
        assert_eq!(d.workspace("w2").unwrap().1.folder, "/srv");
        ok(&mut d, WsOp::DeleteWorkspace { id: s("w3") });
        assert_eq!(order(&d), ["frontend", "api"]);
        assert!(d.pane("w3-p").is_none());
    }

    #[test]
    fn workspaces_move_only_inside_their_project() {
        let mut d = base();
        add_project(&mut d, "p2", "hiero");
        add_ws(&mut d, "p2", "w9", "workspace a");
        ok(&mut d, WsOp::MoveWorkspace { id: s("w1"), dir: Move::Down });
        ok(&mut d, WsOp::MoveWorkspace { id: s("w9"), dir: Move::Up });
        assert_eq!(d.project("p1").unwrap().workspaces.len(), 1);
        assert_eq!(d.project("p2").unwrap().workspaces.len(), 1);
    }

    #[test]
    fn workspace_validation() {
        let mut d = base();
        let add = |name: &str, id: &str, t: &str, p: &str| WsOp::AddWorkspace {
            project: s("p1"),
            id: s(id),
            name: s(name),
            folder: s("/"),
            tab_id: s(t),
            pane_id: s(p),
        };
        assert!(err(&mut d, add("", "w2", "t2", "q2")).contains("empty"));
        assert!(err(&mut d, add("frontend", "w2", "t2", "q2")).contains("already has a workspace"));
        assert!(err(&mut d, add("x", "w1", "t2", "q2")).contains("already used"));
        assert!(err(&mut d, add("x", "w2", "w1-t", "q2")).contains("already used"));
        assert!(err(&mut d, add("x", "w2", "t2", "w1-p")).contains("already used"));
        assert!(err(&mut d, add("x", "w2", "w2", "q2")).contains("used twice"));
        assert!(err(&mut d, add("x", "w2", "t2", "t2")).contains("used twice"));
        assert!(
            err(
                &mut d,
                WsOp::AddWorkspace {
                    project: s("nope"),
                    id: s("w2"),
                    name: s("x"),
                    folder: s("/"),
                    tab_id: s("t2"),
                    pane_id: s("q2")
                }
            )
            .contains("no project")
        );
        // The same name is fine in another project.
        add_project(&mut d, "p2", "hiero");
        add_ws(&mut d, "p2", "w5", "frontend");
        add_ws(&mut d, "p1", "w6", "backend");
        assert!(err(&mut d, WsOp::RenameWorkspace { id: s("w6"), name: s("frontend") }).contains("already has"));
        ok(&mut d, WsOp::RenameWorkspace { id: s("w6"), name: s("backend") });
        assert!(err(&mut d, WsOp::RenameWorkspace { id: s("w6"), name: s("  ") }).contains("empty"));
        assert!(err(&mut d, WsOp::RenameWorkspace { id: s("zz"), name: s("a") }).contains("no workspace"));
        assert!(err(&mut d, WsOp::DeleteWorkspace { id: s("zz") }).contains("no workspace"));
        assert!(err(&mut d, WsOp::MoveWorkspace { id: s("zz"), dir: Move::Up }).contains("no workspace"));
        assert!(err(&mut d, WsOp::SetFolder { id: s("zz"), folder: s("/") }).contains("no workspace"));
    }

    #[test]
    fn tab_ops() {
        let mut d = base();
        let add = |id: &str, name: &str, pane: &str| WsOp::AddTab {
            workspace: s("w1"),
            id: s(id),
            name: s(name),
            pane_id: s(pane),
        };
        ok(&mut d, add("t2", "nvim", "q2"));
        ok(&mut d, add("t3", "claude", "q3"));
        let w = d.workspace("w1").unwrap().1;
        assert_eq!(w.tabs.len(), 3);
        assert_eq!(w.active_tab, 2, "a new tab is shown");
        ok(&mut d, WsOp::SetActiveTab { workspace: s("w1"), index: 0 });
        ok(&mut d, WsOp::RenameTab { id: s("t2"), name: s(" editor ") });
        assert_eq!(d.tab("t2").unwrap().1.name, "editor");
        ok(&mut d, WsOp::RenameTab { id: s("t2"), name: s("editor") });

        // Closing a tab before the active one keeps the same tab active.
        ok(&mut d, WsOp::SetActiveTab { workspace: s("w1"), index: 2 });
        ok(&mut d, WsOp::CloseTab { id: s("w1-t") });
        assert_eq!(d.workspace("w1").unwrap().1.active_tab, 1);
        assert_eq!(d.workspace("w1").unwrap().1.tabs[1].id, "t3");
        // Closing the active last tab moves to the new last one.
        ok(&mut d, WsOp::CloseTab { id: s("t3") });
        assert_eq!(d.workspace("w1").unwrap().1.active_tab, 0);
        assert_eq!(d.workspace("w1").unwrap().1.tabs.len(), 1);
    }

    #[test]
    fn closing_the_active_middle_tab_keeps_the_index() {
        let mut d = base();
        for (t, q) in [("t2", "q2"), ("t3", "q3")] {
            ok(&mut d, WsOp::AddTab { workspace: s("w1"), id: s(t), name: s(t), pane_id: s(q) });
        }
        ok(&mut d, WsOp::SetActiveTab { workspace: s("w1"), index: 1 });
        ok(&mut d, WsOp::CloseTab { id: s("t2") });
        let w = d.workspace("w1").unwrap().1;
        assert_eq!(w.tabs[w.active_tab].id, "t3");
    }

    #[test]
    fn tab_validation() {
        let mut d = base();
        let add = |id: &str, name: &str, pane: &str| WsOp::AddTab {
            workspace: s("w1"),
            id: s(id),
            name: s(name),
            pane_id: s(pane),
        };
        assert!(err(&mut d, add("t2", " ", "q2")).contains("empty"));
        assert!(err(&mut d, add("t2", "shell", "q2")).contains("already has a tab"));
        assert!(err(&mut d, add("w1", "x", "q2")).contains("already used"));
        assert!(err(&mut d, add("t2", "x", "w1-p")).contains("already used"));
        assert!(err(&mut d, add("t2", "x", "t2")).contains("used twice"));
        assert!(
            err(&mut d, WsOp::AddTab { workspace: s("zz"), id: s("t2"), name: s("x"), pane_id: s("q2") })
                .contains("no workspace")
        );
        ok(&mut d, add("t2", "other", "q2"));
        assert!(err(&mut d, WsOp::RenameTab { id: s("t2"), name: s("shell") }).contains("already has a tab"));
        assert!(err(&mut d, WsOp::RenameTab { id: s("t2"), name: s("") }).contains("empty"));
        assert!(err(&mut d, WsOp::RenameTab { id: s("zz"), name: s("a") }).contains("no tab"));
        assert!(err(&mut d, WsOp::SetActiveTab { workspace: s("w1"), index: 2 }).contains("doesn't exist"));
        assert!(err(&mut d, WsOp::SetActiveTab { workspace: s("zz"), index: 0 }).contains("no workspace"));
        assert!(err(&mut d, WsOp::CloseTab { id: s("zz") }).contains("no tab"));
    }

    #[test]
    fn the_last_tab_cannot_be_closed() {
        let mut d = base();
        assert!(err(&mut d, WsOp::CloseTab { id: s("w1-t") }).contains("last tab"));
        // Neither by closing its last pane.
        assert!(err(&mut d, WsOp::ClosePane { pane: s("w1-p") }).contains("last tab"));
        assert_eq!(d.workspace("w1").unwrap().1.tabs.len(), 1);
    }

    #[test]
    fn split_replaces_a_pane_with_two() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        let tab = d.tab("w1-t").unwrap().1;
        match &tab.layout {
            Layout::Split { dir, children, sizes } => {
                assert_eq!(*dir, Dir::Horizontal);
                assert_eq!(children.len(), 2);
                assert_eq!(sizes, &[1, 1]);
            }
            _ => panic!("expected a split"),
        }
        assert_eq!(pane_ids(&d, "w1-t"), ["w1-p", "b"]);
        assert_eq!(tab.focus, "b", "the new pane gets the focus");
    }

    #[test]
    fn split_in_the_parents_direction_adds_a_sibling() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Horizontal, "c"));
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "d"));
        let Layout::Split { children, sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(children.len(), 4, "no nesting");
        assert!(children.iter().all(|c| matches!(c, Layout::Pane(_))));
        assert_eq!(pane_ids(&d, "w1-t"), ["w1-p", "d", "b", "c"]);
        assert_eq!(sizes.len(), 4);
        assert!(sizes.iter().all(|&x| x >= 1));
    }

    #[test]
    fn split_in_the_other_direction_nests() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Vertical, "c"));
        let Layout::Split { children, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(children.len(), 2);
        let Layout::Split { dir, children: inner, .. } = &children[1] else { panic!("nested split") };
        assert_eq!(*dir, Dir::Vertical);
        assert_eq!(inner.len(), 2);
        assert_eq!(pane_ids(&d, "w1-t"), ["w1-p", "b", "c"]);
        // Splitting the nested one again, the same way, adds a sibling there.
        ok(&mut d, split_op("c", Dir::Vertical, "e"));
        assert_eq!(pane_ids(&d, "w1-t"), ["w1-p", "b", "c", "e"]);
    }

    #[test]
    fn split_halves_the_weight_and_inherits_the_folder() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        let Layout::Split { sizes, .. } = &mut d.projects[0].workspaces[0].tabs[0].layout else { panic!() };
        sizes[0] = 7;
        sizes[1] = 3;
        if let Some((_, _, _)) = d.pane("b") {
            let p = d.projects[0].workspaces[0].tabs[0].layout.find_mut("b").unwrap();
            p.cwd = Some(s("/elsewhere"));
        }
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "c"));
        ok(&mut d, split_op("b", Dir::Horizontal, "e"));
        let Layout::Split { sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        // 7 -> 4 + 3, then 3 -> 2 + 1.
        assert_eq!(sizes, &[4, 3, 2, 1]);
        assert_eq!(d.pane("e").unwrap().2.cwd.as_deref(), Some("/elsewhere"));
        assert_eq!(d.pane("c").unwrap().2.command, None);
    }

    #[test]
    fn split_validation() {
        let mut d = base();
        assert!(err(&mut d, split_op("zz", Dir::Vertical, "b")).contains("no pane"));
        assert!(err(&mut d, split_op("w1-p", Dir::Vertical, "w1-p")).contains("already used"));
        assert!(err(&mut d, split_op("w1-p", Dir::Vertical, "")).contains("empty"));
    }

    #[test]
    fn closing_a_pane_collapses_the_split() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, WsOp::ClosePane { pane: s("b") });
        assert_eq!(d.tab("w1-t").unwrap().1.layout, Layout::Pane(Pane::shell("w1-p")));
        assert_eq!(d.tab("w1-t").unwrap().1.focus, "w1-p");
    }

    #[test]
    fn closing_in_a_nested_split_merges_levels() {
        let mut d = base();
        // [a | [b / c]]; closing b leaves [a | c].
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Vertical, "c"));
        ok(&mut d, WsOp::ClosePane { pane: s("b") });
        let Layout::Split { dir, children, sizes } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(*dir, Dir::Horizontal);
        assert_eq!(children.len(), 2);
        assert_eq!(sizes.len(), 2);
        assert!(children.iter().all(|c| matches!(c, Layout::Pane(_))));
        assert_eq!(pane_ids(&d, "w1-t"), ["w1-p", "c"]);
    }

    #[test]
    fn collapsing_a_split_merges_equal_directions_upward() {
        let mut d = base();
        // H[a, V[H[b, c], x]]: closing x makes V a single child H[b, c], which joins the outer H.
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "x0"));
        ok(&mut d, split_op("x0", Dir::Vertical, "x"));
        ok(&mut d, split_op("x0", Dir::Horizontal, "c"));
        ok(&mut d, WsOp::ClosePane { pane: s("x") });
        let Layout::Split { dir, children, sizes } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(*dir, Dir::Horizontal);
        assert_eq!(children.len(), 3, "flattened: {children:?}");
        assert!(children.iter().all(|c| matches!(c, Layout::Pane(_))));
        assert_eq!(sizes.len(), 3);
        assert_eq!(pane_ids(&d, "w1-t"), ["w1-p", "x0", "c"]);
    }

    #[test]
    fn closing_moves_the_focus_to_the_neighbour() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Horizontal, "c"));
        // order: w1-p b c; focus is c (the newest).
        ok(&mut d, WsOp::SetFocus { pane: s("b") });
        ok(&mut d, WsOp::ClosePane { pane: s("b") });
        assert_eq!(d.tab("w1-t").unwrap().1.focus, "c");
        ok(&mut d, WsOp::ClosePane { pane: s("c") });
        assert_eq!(d.tab("w1-t").unwrap().1.focus, "w1-p");
        // Closing a pane that isn't focused leaves the focus alone.
        ok(&mut d, split_op("w1-p", Dir::Vertical, "e"));
        ok(&mut d, WsOp::SetFocus { pane: s("w1-p") });
        ok(&mut d, WsOp::ClosePane { pane: s("e") });
        assert_eq!(d.tab("w1-t").unwrap().1.focus, "w1-p");
    }

    #[test]
    fn closing_the_last_pane_of_a_tab_closes_the_tab() {
        let mut d = base();
        ok(&mut d, WsOp::AddTab { workspace: s("w1"), id: s("t2"), name: s("nvim"), pane_id: s("q2") });
        ok(&mut d, WsOp::ClosePane { pane: s("q2") });
        let w = d.workspace("w1").unwrap().1;
        assert_eq!(w.tabs.len(), 1);
        assert_eq!(w.tabs[0].id, "w1-t");
        assert_eq!(w.active_tab, 0);
        assert!(d.pane("q2").is_none());
    }

    #[test]
    fn close_pane_validation() {
        let mut d = base();
        assert!(err(&mut d, WsOp::ClosePane { pane: s("zz") }).contains("no pane"));
        assert!(err(&mut d, WsOp::SetFocus { pane: s("zz") }).contains("no pane"));
        assert!(err(&mut d, WsOp::SetTerminal { pane: s("zz"), terminal: None }).contains("no pane"));
        assert!(err(&mut d, WsOp::SetCommand { pane: s("zz"), command: None }).contains("no pane"));
    }

    #[test]
    fn rename_pane() {
        let mut d = WsDoc::default();
        ok(&mut d, WsOp::AddProject { id: s("p"), name: s("p"), root: s("/r") });
        ok(&mut d, WsOp::AddWorkspace { project: s("p"), id: s("w"), name: s("w"), folder: s("/r"), tab_id: s("t"), pane_id: s("n") });
        ok(&mut d, WsOp::RenamePane { pane: s("n"), name: Some(s("  api server ")) });
        assert_eq!(d.pane("n").unwrap().2.name.as_deref(), Some("api server"));
        // Blank or None clears it.
        ok(&mut d, WsOp::RenamePane { pane: s("n"), name: Some(s("  ")) });
        assert_eq!(d.pane("n").unwrap().2.name, None);
        ok(&mut d, WsOp::RenamePane { pane: s("n"), name: Some(s("x")) });
        ok(&mut d, WsOp::RenamePane { pane: s("n"), name: None });
        assert_eq!(d.pane("n").unwrap().2.name, None);
        assert!(err(&mut d, WsOp::RenamePane { pane: s("zz"), name: None }).contains("no pane"));
        // Names round-trip and are left out of the JSON when unset.
        ok(&mut d, WsOp::RenamePane { pane: s("n"), name: Some(s("api")) });
        let v = serde_json::to_value(&d).unwrap();
        assert!(v.to_string().contains("\"name\":\"api\""));
        let back: WsDoc = serde_json::from_value(v).unwrap();
        assert_eq!(back.pane("n").unwrap().2.name.as_deref(), Some("api"));
    }

    #[test]
    fn focus_command_and_terminal() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, WsOp::SetFocus { pane: s("w1-p") });
        assert_eq!(d.tab("w1-t").unwrap().1.focus, "w1-p");
        ok(&mut d, WsOp::SetCommand { pane: s("b"), command: Some(s("nvim .")) });
        ok(&mut d, WsOp::SetTerminal { pane: s("b"), terminal: Some(s("t7")) });
        let p = d.pane("b").unwrap().2;
        assert_eq!(p.command.as_deref(), Some("nvim ."));
        assert_eq!(p.terminal.as_deref(), Some("t7"));
        ok(&mut d, WsOp::SetTerminal { pane: s("b"), terminal: None });
        assert_eq!(d.pane("b").unwrap().2.terminal, None);
    }

    #[test]
    fn panes_of_a_workspace_span_all_tabs() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, WsOp::AddTab { workspace: s("w1"), id: s("t2"), name: s("x"), pane_id: s("q2") });
        let ids: Vec<_> = d.workspace("w1").unwrap().1.panes().iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids, ["w1-p", "b", "q2"]);
        assert_eq!(d.pane("q2").unwrap().0.id, "w1");
        assert_eq!(d.pane("q2").unwrap().1.id, "t2");
    }

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    fn pane(id: &str) -> Layout {
        Layout::Pane(Pane::shell(id))
    }

    /// Every cell of `area` is covered by exactly one rect.
    fn assert_exact_cover(got: &[(String, Rect)], area: Rect) {
        let mut grid = vec![0u8; area.width as usize * area.height as usize];
        for (id, r) in got {
            for y in r.y..r.y + r.height {
                for x in r.x..r.x + r.width {
                    assert!(
                        x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height,
                        "{id} outside the area"
                    );
                    grid[(y - area.y) as usize * area.width as usize + (x - area.x) as usize] += 1;
                }
            }
        }
        assert!(grid.iter().all(|&c| c == 1), "every cell exactly once");
    }

    #[test]
    fn rects_single_pane_fills_the_area() {
        let a = Rect::new(3, 2, 40, 10);
        assert_eq!(rects(&pane("a"), a), vec![(s("a"), a)]);
    }

    #[test]
    fn rects_follow_the_weights() {
        let l = Layout::Split { dir: Dir::Horizontal, children: vec![pane("a"), pane("b")], sizes: vec![1, 3] };
        let r = rects(&l, area(80, 24));
        assert_eq!(r, vec![(s("a"), Rect::new(0, 0, 20, 24)), (s("b"), Rect::new(20, 0, 60, 24))]);
        let l = Layout::Split { dir: Dir::Vertical, children: vec![pane("a"), pane("b")], sizes: vec![1, 1] };
        let r = rects(&l, Rect::new(5, 5, 10, 9));
        assert_eq!(r[0].1, Rect::new(5, 5, 10, 4));
        assert_eq!(r[1].1, Rect::new(5, 9, 10, 5));
        assert_exact_cover(&r, Rect::new(5, 5, 10, 9));
    }

    #[test]
    fn rects_odd_sizes_lose_no_cells() {
        for w in [1u16, 2, 3, 7, 80, 101, 237] {
            for weights in [vec![1, 1, 1], vec![2, 3, 5], vec![1, 100, 1], vec![0, 0, 0], vec![1]] {
                let l = Layout::Split {
                    dir: Dir::Horizontal,
                    children: vec![pane("a"), pane("b"), pane("c")],
                    sizes: weights,
                };
                let a = Rect::new(2, 1, w, 5);
                let r = rects(&l, a);
                assert_eq!(r.len(), 3);
                assert_exact_cover(&r, a);
            }
        }
    }

    #[test]
    fn rects_missing_weights_count_as_one() {
        let l = Layout::Split { dir: Dir::Horizontal, children: vec![pane("a"), pane("b")], sizes: vec![] };
        let r = rects(&l, area(11, 3));
        assert_eq!(r[0].1.width + r[1].1.width, 11);
        assert_eq!(r[0].1.width, 5);
    }

    #[test]
    fn borders_sit_between_neighbours_and_nest() {
        let inner = Layout::Split { dir: Dir::Vertical, children: vec![pane("b"), pane("c")], sizes: vec![1, 2] };
        let l = Layout::Split { dir: Dir::Horizontal, children: vec![pane("a"), inner, pane("d")], sizes: vec![1, 1, 1] };
        let a = Rect::new(0, 0, 90, 30);
        let bs = borders(&l, a);
        assert_eq!(bs.len(), 3);
        assert_eq!((bs[0].path.clone(), bs[0].index, bs[0].at, bs[0].dir), (vec![], 0, 30, Dir::Horizontal));
        assert_eq!((bs[1].path.clone(), bs[1].index, bs[1].at), (vec![], 1, 60));
        assert_eq!((bs[2].path.clone(), bs[2].index, bs[2].at, bs[2].dir), (vec![1], 0, 10, Dir::Vertical));
        // Two cells, along the whole length of the split only.
        assert!(bs[0].contains(29, 0) && bs[0].contains(30, 29));
        assert!(!bs[0].contains(28, 5) && !bs[0].contains(31, 5) && !bs[0].contains(30, 30));
        assert!(bs[2].contains(45, 9) && bs[2].contains(45, 10));
        assert!(!bs[2].contains(29, 10), "outside the inner split");
        assert!(borders(&pane("a"), a).is_empty());
    }

    #[test]
    fn drag_sizes_follow_the_pointer_and_keep_five_percent() {
        let l = Layout::Split { dir: Dir::Horizontal, children: vec![pane("a"), pane("b"), pane("c")], sizes: vec![1, 1, 1] };
        let a = Rect::new(10, 0, 90, 30);
        let b = borders(&l, a)[0].clone(); // between a and b: at = 40
        assert_eq!(drag_sizes(&l, &b, 50), Some(vec![40, 20, 30]), "c keeps its cells");
        assert_eq!(drag_sizes(&l, &b, 40), Some(vec![30, 30, 30]));
        // Never closer than 5% (5 cells of 90, rounded up) to the neighbours' far edges.
        assert_eq!(drag_sizes(&l, &b, 0), Some(vec![5, 55, 30]));
        assert_eq!(drag_sizes(&l, &b, 500), Some(vec![55, 5, 30]));
        let b2 = borders(&l, a)[1].clone();
        assert_eq!(drag_sizes(&l, &b2, 500), Some(vec![30, 55, 5]));
        // Vertical splits use rows.
        let v = Layout::Split { dir: Dir::Vertical, children: vec![pane("a"), pane("b")], sizes: vec![] };
        let vb = borders(&v, Rect::new(0, 2, 40, 20))[0].clone();
        assert_eq!(drag_sizes(&v, &vb, 7), Some(vec![5, 15]));
        // A split that is not there (the layout changed under the drag).
        assert_eq!(drag_sizes(&pane("a"), &b, 50), None);
    }

    #[test]
    fn set_sizes_sets_one_split() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Vertical, "c"));
        let tab = s("w1-t");
        let op = |path: Vec<usize>, sizes: Vec<u16>| WsOp::SetSizes { tab: tab.clone(), path, sizes };
        ok(&mut d, op(vec![], vec![30, 70]));
        ok(&mut d, op(vec![1], vec![1, 3]));
        let Layout::Split { sizes, children, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(sizes, &vec![30, 70]);
        let Layout::Split { sizes: inner, .. } = &children[1] else { panic!() };
        assert_eq!(inner, &vec![1, 3]);
        assert_eq!(widths(&d, "w1-t")[0], ("w1-p".to_string(), 30, 40));
        // Refused: wrong count, zero weight, not a split, no such path or tab.
        assert!(err(&mut d, op(vec![], vec![1, 2, 3])).contains("2 children"));
        assert!(err(&mut d, op(vec![], vec![0, 5])).contains("no share"));
        assert!(err(&mut d, op(vec![0], vec![1])).contains("no split"));
        assert!(err(&mut d, op(vec![7], vec![1])).contains("no split"));
        assert!(err(&mut d, WsOp::SetSizes { tab: s("nope"), path: vec![], sizes: vec![1, 1] }).contains("no tab"));
        // Nothing changed by the refusals.
        let Layout::Split { sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(sizes, &vec![30, 70]);
    }

    #[test]
    fn rects_nested() {
        let inner = Layout::Split { dir: Dir::Vertical, children: vec![pane("b"), pane("c")], sizes: vec![1, 2] };
        let l = Layout::Split { dir: Dir::Horizontal, children: vec![pane("a"), inner], sizes: vec![1, 1] };
        let a = Rect::new(0, 0, 81, 31);
        let r = rects(&l, a);
        assert_eq!(r.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(r[0].1, Rect::new(0, 0, 40, 31));
        assert_eq!(r[1].1, Rect::new(40, 0, 41, 10));
        assert_eq!(r[2].1, Rect::new(40, 10, 41, 21));
        assert_exact_cover(&r, a);
    }

    fn do_resize(d: &mut WsDoc, pane: &str, dir: Dir, delta: i16) {
        let tab = d.pane(pane).unwrap().1.id.clone();
        ok(d, WsOp::ResizePane { tab, pane: s(pane), dir, delta });
    }

    fn widths(d: &WsDoc, tab: &str) -> Vec<(String, u16, u16)> {
        let l = &d.tab(tab).unwrap().1.layout;
        rects(l, Rect::new(0, 0, 100, 40)).into_iter().map(|(id, r)| (id, r.width, r.height)).collect()
    }

    #[test]
    fn resize_grows_one_side_and_shrinks_its_neighbour() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        let before = widths(&d, "w1-t");
        assert_eq!(before, [(s("w1-p"), 50, 40), (s("b"), 50, 40)]);
        do_resize(&mut d, "w1-p", Dir::Horizontal, 10);
        let w = widths(&d, "w1-t");
        assert!(w[0].1 > 50 && w[1].1 < 50 && w[0].1 + w[1].1 == 100, "{w:?}");
        // Shrinking the last pane gives the room to the previous one.
        do_resize(&mut d, "b", Dir::Horizontal, -10);
        let w2 = widths(&d, "w1-t");
        assert!(w2[1].1 < w[1].1, "{w2:?}");
        // The other axis has no split here: nothing changes.
        let snapshot = d.clone();
        do_resize(&mut d, "b", Dir::Vertical, 10);
        assert_eq!(d, snapshot);
    }

    #[test]
    fn resize_picks_the_nearest_split_of_that_direction() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Vertical, "c"));
        // a | (b over c). Vertical from b moves the inner border only.
        do_resize(&mut d, "b", Dir::Vertical, 10);
        let w = widths(&d, "w1-t");
        assert_eq!(w[0], (s("w1-p"), 50, 40));
        assert!(w[1].2 > 20 && w[2].2 < 20, "{w:?}");
        // Horizontal from b goes past the inner split to the outer one.
        do_resize(&mut d, "b", Dir::Horizontal, 10);
        let w = widths(&d, "w1-t");
        assert!(w[0].1 < 50 || w[1].1 > 50, "{w:?}");
        assert_eq!(w[1].1, w[2].1);
        // ...and from the left pane the same outer split grows it.
        do_resize(&mut d, "w1-p", Dir::Horizontal, 20);
        assert!(widths(&d, "w1-t")[0].1 > w[0].1);
    }

    #[test]
    fn resize_weights_never_drop_below_one_and_stop_at_the_limit() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Horizontal, "c"));
        for _ in 0..60 {
            do_resize(&mut d, "w1-p", Dir::Horizontal, 7);
        }
        let Layout::Split { sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert!(sizes.iter().all(|&x| x >= 1), "{sizes:?}");
        assert_eq!(sizes.len(), 3);
        // The next pane gave all it could; asking for more is a no-op.
        let snapshot = d.clone();
        do_resize(&mut d, "w1-p", Dir::Horizontal, 7);
        assert_eq!(d, snapshot);
        // Shrinking to the floor stops too.
        for _ in 0..60 {
            do_resize(&mut d, "w1-p", Dir::Horizontal, -7);
        }
        let Layout::Split { sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert!(sizes.iter().all(|&x| x >= 1));
        let snapshot = d.clone();
        do_resize(&mut d, "w1-p", Dir::Horizontal, -7);
        assert_eq!(d, snapshot);
        // Every pane still gets cells.
        assert!(widths(&d, "w1-t").iter().all(|w| w.1 >= 1));
    }

    #[test]
    fn resize_checks_its_ids() {
        let mut d = base();
        add_ws(&mut d, "p1", "w2", "backend");
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        let e = err(&mut d, WsOp::ResizePane { tab: s("w2-t"), pane: s("b"), dir: Dir::Horizontal, delta: 5 });
        assert!(e.contains("not in tab"), "{e}");
        let e = err(&mut d, WsOp::ResizePane { tab: s("nope"), pane: s("b"), dir: Dir::Horizontal, delta: 5 });
        assert!(e.contains("no tab"), "{e}");
    }

    #[test]
    fn rects_tiny_and_empty_areas_do_not_panic() {
        let l = Layout::Split { dir: Dir::Horizontal, children: vec![pane("a"), pane("b")], sizes: vec![1, 1] };
        for a in [area(0, 0), area(1, 1), area(0, 5)] {
            let r = rects(&l, a);
            assert_eq!(r.len(), 2);
            assert_exact_cover(&r, a);
        }
    }

    #[test]
    fn rects_of_a_real_tab() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Vertical, "c"));
        ok(&mut d, split_op("c", Dir::Horizontal, "e"));
        let a = area(123, 47);
        let r = rects(&d.tab("w1-t").unwrap().1.layout, a);
        assert_eq!(r.len(), 4);
        assert_exact_cover(&r, a);
    }

    fn term(addr: &str, state: Option<&str>) -> hc_client::Terminal {
        let mut v = json!({"address": addr});
        if let Some(st) = state {
            v["status"] = json!({"state": st});
        }
        serde_json::from_value(v).expect("terminal json")
    }

    fn set_of(list: &[(&str, Option<&str>)]) -> TerminalSet {
        let mut set = TerminalSet::new();
        set.reset(1, list.iter().map(|(a, s)| term(a, *s)).collect());
        set
    }

    /// Workspace with panes a, b, c on terminals t1, t2, t3 (c has none).
    fn status_doc() -> WsDoc {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Horizontal, "c"));
        ok(&mut d, WsOp::SetTerminal { pane: s("w1-p"), terminal: Some(s("t1")) });
        ok(&mut d, WsOp::SetTerminal { pane: s("b"), terminal: Some(s("t2")) });
        d
    }

    #[test]
    fn status_priority() {
        let d = status_doc();
        let w = d.workspace("w1").unwrap().1;
        let st = |a: Option<&str>, b: Option<&str>| workspace_status(w, &set_of(&[("t1", a), ("t2", b)]));
        assert_eq!(st(None, None), WsStatus::None);
        assert_eq!(st(Some("idle"), None), WsStatus::Idle);
        assert_eq!(st(Some("idle"), Some("done")), WsStatus::Done);
        assert_eq!(st(Some("done"), Some("working")), WsStatus::Working);
        assert_eq!(st(Some("working"), Some("blocked")), WsStatus::Attention);
        assert_eq!(st(Some("error"), Some("working")), WsStatus::Attention);
        assert_eq!(st(Some("blocked"), Some("error")), WsStatus::Attention);
        assert!(WsStatus::Attention > WsStatus::Working && WsStatus::Working > WsStatus::Done);
        assert!(WsStatus::Done > WsStatus::Idle && WsStatus::Idle > WsStatus::None);
    }

    #[test]
    fn status_ignores_missing_terminals() {
        let d = status_doc();
        let w = d.workspace("w1").unwrap().1;
        // t1 and t2 are not in the set (the terminals are gone).
        assert_eq!(workspace_status(w, &set_of(&[("t9", Some("blocked"))])), WsStatus::None);
        assert_eq!(workspace_status(w, &TerminalSet::new()), WsStatus::None);
        // A pane without an address counts as nothing.
        assert_eq!(pane_status(&Pane::shell("x"), &set_of(&[("t1", Some("working"))])), WsStatus::None);
    }

    #[test]
    fn status_spans_tabs_and_projects() {
        let mut d = status_doc();
        ok(&mut d, WsOp::AddTab { workspace: s("w1"), id: s("t2x"), name: s("claude"), pane_id: s("q2") });
        ok(&mut d, WsOp::SetTerminal { pane: s("q2"), terminal: Some(s("t3")) });
        add_project(&mut d, "p2", "empty");
        let set = set_of(&[("t1", Some("idle")), ("t3", Some("blocked"))]);
        assert_eq!(workspace_status(d.workspace("w1").unwrap().1, &set), WsStatus::Attention);
        assert_eq!(project_status(d.project("p1").unwrap(), &set), WsStatus::Attention);
        assert_eq!(project_status(d.project("p2").unwrap(), &set), WsStatus::None);
    }

    #[test]
    fn serde_round_trip_keeps_unknown_keys() {
        let mut d = WsDoc::from_value(&json!({"version": 3, "projects": [], "note": {"a": [1, 2]}}));
        add_project(&mut d, "p1", "shosha");
        add_ws(&mut d, "p1", "w1", "frontend");
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, split_op("b", Dir::Vertical, "c"));
        ok(&mut d, WsOp::SetCommand { pane: s("c"), command: Some(s("claude")) });
        ok(&mut d, WsOp::SetTerminal { pane: s("c"), terminal: Some(s("t4")) });
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["version"], 3);
        assert_eq!(v["note"], json!({"a": [1, 2]}));
        assert_eq!(WsDoc::from_value(&v), d);
        // Through text too.
        let text = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<WsDoc>(&text).unwrap(), d);
    }

    #[test]
    fn json_format() {
        let mut d = WsDoc::default();
        add_project(&mut d, "p1", "hiero");
        add_ws(&mut d, "p1", "w1", "workspace a");
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        let v = serde_json::to_value(&d).unwrap();
        let tab = &v["projects"][0]["workspaces"][0]["tabs"][0];
        assert_eq!(
            tab["layout"],
            json!({
                "type": "split",
                "dir": "horizontal",
                "children": [
                    {"type": "pane", "id": "w1-p"},
                    {"type": "pane", "id": "b"}
                ],
                "sizes": [1, 1]
            })
        );
        assert_eq!(tab["focus"], "b");
    }

    #[test]
    fn reading_is_forgiving() {
        // Missing optional fields get defaults.
        let d = WsDoc::from_value(&json!({
            "projects": [{"id": "p", "name": "n", "workspaces": [{"id": "w", "name": "x", "tabs": [
                {"id": "t", "name": "shell", "layout": {"type": "pane", "id": "a"}}
            ]}]}]
        }));
        let (_, w) = d.workspace("w").unwrap();
        assert_eq!(w.active_tab, 0);
        assert_eq!(w.folder, "");
        assert!(!d.projects[0].collapsed);
        assert_eq!(d.pane("a").unwrap().2.terminal, None);
        // Null and garbage give an empty doc rather than an error.
        assert!(WsDoc::from_value(&Value::Null).projects.is_empty());
        assert!(WsDoc::from_value(&json!({"projects": "nope"})).projects.is_empty());
        // A split without sizes still works.
        let l: Layout = serde_json::from_value(json!({
            "type": "split", "dir": "vertical",
            "children": [{"type": "pane", "id": "a"}, {"type": "pane", "id": "b"}]
        }))
        .unwrap();
        assert_eq!(rects(&l, area(4, 10)).len(), 2);
    }

    #[test]
    fn new_ids_are_distinct() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_id("w")).collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.iter().all(|i| i.starts_with("w-")));
    }

    #[test]
    fn tree_text() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        ok(&mut d, WsOp::SetCommand { pane: s("b"), command: Some(s("nvim")) });
        ok(&mut d, WsOp::SetTerminal { pane: s("b"), terminal: Some(s("t3")) });
        let t = render_tree(&d);
        assert!(t.starts_with("shosha  /r\n  frontend  /r/f\n    tab shell *\n"), "{t}");
        assert!(t.contains("split horizontal [1:1]"));
        assert!(t.contains("pane b  nvim  terminal t3 *"));
        assert!(render_tree(&WsDoc::default()).contains("no projects"));
    }

    #[test]
    fn set_layout_puts_back_a_saved_layout_with_its_sizes() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "a"));
        ok(&mut d, WsOp::SetSizes { tab: s("w1-t"), path: vec![], sizes: vec![4, 3] });
        ok(&mut d, WsOp::RenamePane { pane: s("a"), name: Some(s("old")) });
        let saved = d.tab("w1-t").unwrap().1.layout.clone();
        // A debug pane comes and goes; the sizes don't come back by themselves.
        ok(&mut d, split_op("a", Dir::Vertical, "dbg"));
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "dbg2"));
        ok(&mut d, WsOp::RenamePane { pane: s("a"), name: Some(s("new")) });
        ok(&mut d, WsOp::ClosePane { pane: s("dbg") });
        ok(&mut d, WsOp::ClosePane { pane: s("dbg2") });
        assert_ne!(d.tab("w1-t").unwrap().1.layout, saved);
        ok(&mut d, WsOp::SetLayout { tab: s("w1-t"), layout: saved.clone() });
        let Layout::Split { sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(sizes, &[4, 3]);
        // The panes keep what they have now.
        assert_eq!(d.pane("a").unwrap().2.name.as_deref(), Some("new"));
        // Not with other panes than the tab has.
        ok(&mut d, split_op("a", Dir::Vertical, "extra"));
        assert!(err(&mut d, WsOp::SetLayout { tab: s("w1-t"), layout: saved }).contains("changed"));
    }

    #[test]
    fn set_layout_brings_back_hidden_panes() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "shell"));
        ok(&mut d, WsOp::SetTerminal { pane: s("shell"), terminal: Some(s("t9")) });
        ok(&mut d, WsOp::SetSizes { tab: s("w1-t"), path: vec![], sizes: vec![5, 3] });
        ok(&mut d, WsOp::SetFocus { pane: s("w1-p") });
        let saved = d.tab("w1-t").unwrap().1.layout.clone();
        // Hidden for a debug session: out of the tab, its terminal untouched.
        ok(&mut d, WsOp::HideOthers { pane: s("w1-p") });
        let t = d.tab("w1-t").unwrap().1;
        assert_eq!((t.layout.panes().len(), t.hidden.len(), t.hidden_by.as_deref()), (1, 1, Some("w1-p")));
        ok(&mut d, split_op("w1-p", Dir::Vertical, "dbg"));
        ok(&mut d, WsOp::ClosePane { pane: s("dbg") });
        ok(&mut d, WsOp::SetLayout { tab: s("w1-t"), layout: saved.clone() });
        let t = d.tab("w1-t").unwrap().1;
        assert_eq!(t.layout, saved);
        assert!(t.hidden.is_empty() && t.hidden_by.is_none());
        assert_eq!(d.pane("shell").unwrap().2.terminal.as_deref(), Some("t9"));
        assert_eq!(t.focus, "w1-p");
        // Not when a pane of the layout lives in another tab now.
        add_ws(&mut d, "p1", "w2", "backend");
        ok(&mut d, WsOp::ClosePane { pane: s("shell") });
        let mut other = saved.clone();
        if let Layout::Split { children, .. } = &mut other {
            children[1] = Layout::Pane(Pane::shell("w2-p"));
        }
        assert!(err(&mut d, WsOp::SetLayout { tab: s("w1-t"), layout: other }).contains("another tab"));
        // without_pane drops one and keeps the rest.
        assert_eq!(without_pane(&saved, "shell").panes().len(), 1);
    }

    #[test]
    fn hidden_panes_come_back_when_the_pane_that_hid_them_closes() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "shell"));
        ok(&mut d, split_op("shell", Dir::Vertical, "logs"));
        ok(&mut d, WsOp::SetTerminal { pane: s("logs"), terminal: Some(s("t5")) });
        ok(&mut d, WsOp::HideOthers { pane: s("w1-p") });
        assert_eq!(pane_ids(&d, "w1-t"), vec!["w1-p"]);
        // Their ids stay taken while hidden.
        assert!(err(&mut d, split_op("w1-p", Dir::Horizontal, "logs")).contains("logs"));
        // nvim's pane closes (it was the only one on screen): the tab stays, with them.
        ok(&mut d, WsOp::ClosePane { pane: s("w1-p") });
        let t = d.tab("w1-t").unwrap().1;
        assert_eq!(pane_ids(&d, "w1-t"), vec!["shell", "logs"]);
        assert!(t.hidden.is_empty() && t.hidden_by.is_none());
        assert_eq!(d.pane("logs").unwrap().2.terminal.as_deref(), Some("t5"));
        // Unhide puts them beside what is on screen, across its direction.
        ok(&mut d, split_op("shell", Dir::Horizontal, "x"));
        ok(&mut d, WsOp::HideOthers { pane: s("x") });
        ok(&mut d, split_op("x", Dir::Horizontal, "dbg"));
        ok(&mut d, WsOp::Unhide { tab: s("w1-t") });
        let Layout::Split { dir, children, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!((*dir, children.len()), (Dir::Vertical, 3));
    }

    fn req(dir: Dir) -> SplitRequest {
        SplitRequest { dir, size: None, name: None, command: None, no_focus: false, reuse: false }
    }

    fn planned(d: &mut WsDoc, target: &str, new_id: &str, r: &SplitRequest) -> (String, Option<String>) {
        let (ops, id, old) = plan_split(d, target, new_id, r).unwrap();
        for op in &ops {
            apply(d, op).unwrap();
        }
        (id, old)
    }

    #[test]
    fn pane_split_sets_command_name_and_focuses_the_new_pane() {
        let mut d = base();
        let r = SplitRequest { name: Some(s(" out ")), command: Some(s("make run")), ..req(Dir::Vertical) };
        let (id, old) = planned(&mut d, "w1-p", "b", &r);
        assert_eq!((id.as_str(), old), ("b", None));
        let p = d.pane("b").unwrap().2;
        assert_eq!((p.name.as_deref(), p.command.as_deref()), (Some("out"), Some("make run")));
        assert_eq!(d.tab("w1-t").unwrap().1.focus, "b");
        let Layout::Split { dir, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(*dir, Dir::Vertical);
    }

    #[test]
    fn pane_split_no_focus_keeps_the_previous_focus() {
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "a"));
        ok(&mut d, WsOp::SetFocus { pane: s("w1-p") });
        planned(&mut d, "a", "b", &SplitRequest { no_focus: true, ..req(Dir::Horizontal) });
        assert_eq!(d.tab("w1-t").unwrap().1.focus, "w1-p");
    }

    #[test]
    fn pane_split_size_is_the_new_panes_share() {
        let mut d = base();
        planned(&mut d, "w1-p", "b", &SplitRequest { size: Some(30), ..req(Dir::Horizontal) });
        let Layout::Split { sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(sizes, &[7, 3]);
        // Joining a split of three (the old split gives its panes 1 and 3): the new pane gets a quarter, the others keep their ratio.
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "a"));
        ok(&mut d, WsOp::SetSizes { tab: s("w1-t"), path: vec![], sizes: vec![1, 3] });
        planned(&mut d, "w1-p", "b", &SplitRequest { size: Some(25), ..req(Dir::Horizontal) });
        let Layout::Split { sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(sizes.len(), 3);
        let total: u32 = sizes.iter().map(|&x| x as u32).sum();
        assert_eq!(sizes[1] as u32 * 4, total, "{sizes:?}");
        assert_eq!(sizes[0] * 3, sizes[2], "{sizes:?}");
        // A nested split is addressed by its path.
        let mut d = base();
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "a"));
        planned(&mut d, "a", "b", &SplitRequest { size: Some(10), ..req(Dir::Vertical) });
        let Layout::Split { children, sizes, .. } = &d.tab("w1-t").unwrap().1.layout else { panic!() };
        assert_eq!(sizes, &[1, 1], "the outer split is untouched");
        let Layout::Split { sizes, .. } = &children[1] else { panic!() };
        assert_eq!(sizes, &[9, 1]);
    }

    #[test]
    fn share_for_keeps_proportions_and_fits_u16() {
        assert_eq!(share_for(&[1, 1], 1, 50), [1, 1]);
        assert_eq!(share_for(&[4, 2, 1], 2, 50), [2, 1, 3]);
        let v = share_for(&[60000, 5000, 1], 2, 90);
        assert!(v.iter().all(|&x| x >= 1));
        assert!(v[2] > v[0], "{v:?}");
    }

    #[test]
    fn pane_split_validation() {
        let mut d = base();
        let e = |d: &WsDoc, t: &str, r: &SplitRequest| plan_split(d, t, "b", r).unwrap_err();
        assert!(e(&d, "zz", &req(Dir::Horizontal)).contains("no pane"));
        assert!(e(&d, "w1-p", &SplitRequest { size: Some(0), ..req(Dir::Horizontal) }).contains("--size"));
        assert!(e(&d, "w1-p", &SplitRequest { size: Some(91), ..req(Dir::Horizontal) }).contains("--size"));
        assert!(e(&d, "w1-p", &SplitRequest { reuse: true, ..req(Dir::Horizontal) }).contains("--name"));
        assert!(e(&d, "w1-p", &SplitRequest { name: Some(s(" ")), ..req(Dir::Horizontal) }).contains("empty"));
        ok(&mut d, split_op("w1-p", Dir::Horizontal, "b"));
        assert!(plan_split(&d, "w1-p", "b", &req(Dir::Horizontal)).unwrap_err().contains("already used"));
    }

    #[test]
    fn pane_split_reuse_restarts_a_named_pane_of_the_same_workspace() {
        let mut d = base();
        add_ws(&mut d, "p1", "w2", "other");
        let named = |n: &str| SplitRequest { name: Some(s(n)), command: Some(s("new cmd")), reuse: true, ..req(Dir::Horizontal) };
        planned(&mut d, "w1-p", "b", &SplitRequest { name: Some(s("out")), command: Some(s("old")), ..req(Dir::Horizontal) });
        ok(&mut d, WsOp::SetTerminal { pane: s("b"), terminal: Some(s("t5")) });
        // Same name in another workspace is not a match: a normal split.
        let (id, old) = planned(&mut d, "w2-p", "c", &named("out"));
        assert_eq!((id.as_str(), old), ("c", None));
        // In the same workspace the pane is reused: new command, no terminal, old one reported.
        let panes = d.workspace("w1").unwrap().1.panes().len();
        let (id, old) = planned(&mut d, "w1-p", "e", &named("out"));
        assert_eq!((id.as_str(), old.as_deref()), ("b", Some("t5")));
        let p = d.pane("b").unwrap().2;
        assert_eq!((p.command.as_deref(), p.terminal.as_deref()), (Some("new cmd"), None));
        assert_eq!(d.workspace("w1").unwrap().1.panes().len(), panes, "no new pane");
        // Without a match it splits.
        let (id, _) = planned(&mut d, "w1-p", "f", &named("dap"));
        assert_eq!(id, "f");
        assert_eq!(d.pane("f").unwrap().2.name.as_deref(), Some("dap"));
    }

    #[test]
    fn pane_by_name_looks_through_all_tabs() {
        let mut d = base();
        ok(&mut d, WsOp::AddTab { workspace: s("w1"), id: s("t2"), name: s("x"), pane_id: s("q") });
        ok(&mut d, WsOp::RenamePane { pane: s("q"), name: Some(s("repl")) });
        let w = &d.workspace("w1").unwrap().1;
        assert_eq!(w.pane_by_name("repl").map(|p| p.id.as_str()), Some("q"));
        assert!(w.pane_by_name("nope").is_none());
    }
}
