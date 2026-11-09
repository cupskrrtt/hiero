//! Board state and behaviour, kept free of drawing and I/O so it can be tested.

use crate::forms::{
    self, Confirm, FormResult, InputDialog, InputPurpose, SpawnForm, TemplatesView, TextInput,
};
use crate::git::GitCache;
use crate::pipeline::{self, PipelineSpec, Run, RunStatus, Step};
use crate::tasks::{self, Task, TaskOp, TaskStatus};
use crate::worktree;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hc_client::{AgentState, EventData, Labels, RunParams, TemplateInfo, Terminal, TerminalSet, WatchMsg};
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const LABEL_PROJECT: &str = "hiero.project";
pub const LABEL_AGENT: &str = "hiero.agent";
pub const LABEL_TASK: &str = "hiero.task";
/// Display name set from Hiero (hc can't rename a terminal).
pub const LABEL_NAME: &str = "hiero.name";

const SHELLS: &[&str] = &["bash", "zsh", "sh", "fish", "nu", "dash", "ksh", "tcsh", "pwsh"];

#[allow(clippy::large_enum_variant)] // one Mode lives at a time
pub enum Mode {
    Normal,
    Search,
    Help,
    Spawn(Box<SpawnForm>),
    Input(InputDialog),
    Confirm(Confirm),
    Templates(TemplatesView),
    Tasks(TasksView),
    Review(Box<ReviewView>),
    Pipelines(PipelinesView),
    Launch(Box<LaunchForm>),
    /// Claude token usage per agent; None while loading.
    Usage(Option<Vec<crate::usage::Row>>),
    /// Typing into an agent inside the board.
    Pane,
}

pub struct TasksView {
    pub selected: usize,
}

/// How the board groups agents (`p` cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    Project,
    /// Parents with their children indented.
    Tree,
    Flat,
}

pub struct PipelinesView {
    pub specs: Vec<PipelineSpec>,
    pub errors: Vec<String>,
    /// Index into specs, then runs (newest first).
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchField {
    Task,
    Folder,
}

pub struct LaunchForm {
    pub spec: PipelineSpec,
    pub task: TextInput,
    pub folder: TextInput,
    pub field: LaunchField,
    pub error: Option<String>,
}

pub struct ReviewView {
    pub task_id: String,
    pub data: Option<worktree::Review>,
    pub error: Option<String>,
    pub loading: bool,
    pub file: usize,
    pub scroll: u16,
}

/// Work for the event loop to do (it owns the connections).
#[derive(Debug, Clone)]
pub enum Action {
    Run { params: RunParams, send_after: Option<String> },
    Send { addr: String, text: String, force: bool },
    Interrupt(String),
    Kill(String),
    Remove(String),
    Keep(String, bool),
    Label(String, Labels),
    AttachFull(String),
    AttachPane(String),
    LoadTemplates,
    EditTemplate(PathBuf),
    DeleteTemplate(PathBuf),
    /// Start an agent in a new worktree and record the task.
    RunTask { params: RunParams, send_after: Option<String>, spec: forms::TaskSpec },
    LoadDoc,
    TaskOps(Vec<TaskOp>),
    LoadReview(Box<Task>),
    TaskMerge(Box<Task>),
    TaskPr(Box<Task>),
    TaskKeep(Box<Task>),
    TaskDiscard(Box<Task>),
    /// Send one message to several agents.
    Broadcast { addrs: Vec<String>, text: String },
    LoadMachines,
    LaunchRun { spec: PipelineSpec, task: String, folder: String, driver: String },
    /// Start a stage of a run; the driver's job.
    StartStage { run: Box<Run>, stage: usize, templates: Vec<TemplateInfo> },
    EditPipeline(PathBuf),
    LoadUsage(Vec<crate::usage::Query>),
}

pub struct Flash {
    pub msg: String,
    pub at: Instant,
    pub error: bool,
}

/// An agent attached inside the board.
pub struct PaneView {
    pub addr: String,
    pub name: String,
    pub parser: vt100::Parser,
    /// Set when the stream ended; the pane stays until you leave it.
    pub ended: Option<String>,
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

impl PaneView {
    pub fn new(addr: String, name: String, cols: u16, rows: u16) -> Self {
        PaneView {
            addr,
            name,
            parser: vt100::Parser::new(nz(rows), nz(cols), 0),
            ended: None,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(nz(rows), nz(cols));
    }
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
    /// The task this agent works on.
    pub task: Option<String>,
    /// Nesting in the tree view.
    pub depth: u16,
    pub parent: Option<String>,
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
    /// blocked, error, done, exited, failed or pipeline (for `[notify] on`).
    pub what: String,
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
    pub grouping: Grouping,
    pub show_finished: bool,
    /// Address → the attention timestamp the person dismissed.
    pub dismissed: HashMap<String, u64>,
    pub preview: Preview,
    pub entries: Vec<Entry>,
    pub git: GitCache,
    pub flash: Option<Flash>,
    /// Work queued for the event loop.
    pub actions: Vec<Action>,
    pub templates: Vec<TemplateInfo>,
    /// Select this terminal as soon as it shows up (after a spawn).
    pub want_select: Option<String>,
    pub pane: Option<PaneView>,
    /// Where hiero was started; the default folder for new agents.
    pub launch_dir: String,
    /// The terminal Hiero itself runs in, when started inside hc.
    pub self_addr: Option<String>,
    /// Task records and pipeline runs (doc `hiero`).
    pub tasks: tasks::Doc,
    /// This Hiero's id as a pipeline driver.
    pub driver_id: String,
    /// Runs whose next stage is being started right now.
    pub starting_runs: std::collections::HashSet<String>,
    /// Alerts raised outside `on_watch` (pipeline results).
    pub alerts: Vec<Alert>,
    pub machines: Vec<String>,
    /// Typed key → default key (`[keys]`).
    pub key_map: BTreeMap<char, char>,
    /// A config problem to show on the board.
    pub config_error: Option<String>,
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
            grouping: Grouping::Project,
            show_finished: true,
            dismissed: HashMap::new(),
            preview: Preview::default(),
            entries: Vec::new(),
            git: GitCache::default(),
            flash: None,
            actions: Vec::new(),
            templates: Vec::new(),
            want_select: None,
            pane: None,
            launch_dir: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            self_addr: std::env::var("HC_TERMINAL_ID").ok().filter(|s| !s.is_empty()),
            tasks: tasks::Doc::default(),
            driver_id: format!("{}-{:x}", std::process::id(), now_ms() % 0xffff_ffff),
            starting_runs: Default::default(),
            alerts: Vec::new(),
            machines: Vec::new(),
            key_map: BTreeMap::new(),
            config_error: None,
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
                self.set.terminals.values_mut().for_each(status_from_labels);
                self.disconnected = None;
                self.connected_once = true;
                self.preview.dirty = true;
            }
            WatchMsg::Disconnected { error } => {
                self.set.apply_watch(msg);
                self.disconnected = Some(error.clone());
            }
            WatchMsg::Event(e) => {
                if let EventData::Doc { name, .. } = &e.data
                    && name == tasks::DOC_NAME
                {
                    self.actions.push(Action::LoadDoc);
                }
                let addr = e.terminal.clone();
                let before = addr.as_deref().and_then(|a| self.set.get(a)).and_then(attention);
                self.set.apply(e);
                if let Some(t) = addr.as_deref().and_then(|a| self.set.terminals.get_mut(a)) {
                    status_from_labels(t);
                }
                if let Some(a) = addr.as_deref() {
                    if let Some(t) = self.set.get(a) {
                        let after = attention(t);
                        if let Some(now) = after
                            && Some(now) != before
                        {
                            alerts.push(alert_for(t, now));
                            // "done" is already in the turn note.
                            if now != Attention::Done {
                                self.task_note(a, now.word().to_string());
                            }
                        }
                        if let EventData::Turn { turn, state } = &e.data {
                            self.task_note(a, format!("turn {turn} ended: {state}"));
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
        self.drive_pipelines();
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
        if self.self_addr.as_deref() == Some(addr.as_str()) {
            self.preview.addr = Some(addr);
            self.preview.text = "(this is Hiero itself)".into();
            self.preview.error = None;
            return None;
        }
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
        if let Some(f) = &self.flash
            && f.at.elapsed().as_secs() >= if f.error { 6 } else { 3 }
        {
            self.flash = None;
        }
    }

    // ---- keys ---------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if ctrl_c {
            match self.mode {
                Mode::Normal | Mode::Search | Mode::Help => self.quit = true,
                Mode::Pane => {} // goes to the agent
                _ => self.mode = Mode::Normal, // cancel the dialog
            }
            if !matches!(self.mode, Mode::Pane) {
                return;
            }
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = match mode {
            Mode::Help => Mode::Normal,
            // Keys go to the agent; the event loop handles them.
            Mode::Pane => Mode::Pane,
            Mode::Search => self.key_search(key),
            Mode::Normal => self.key_normal(key),
            Mode::Spawn(mut form) => match form.on_key(key) {
                FormResult::Continue => Mode::Spawn(form),
                FormResult::Cancel => Mode::Normal,
                FormResult::Submit => match form.plan() {
                    Ok(plan) => {
                        match plan.task {
                            Some(spec) => {
                                self.flash(&format!("making worktree hiero/{}…", spec.slug));
                                self.actions.push(Action::RunTask {
                                    params: plan.params,
                                    send_after: plan.send_after,
                                    spec,
                                });
                            }
                            None => {
                                self.flash("starting…");
                                self.actions.push(Action::Run {
                                    params: plan.params,
                                    send_after: plan.send_after,
                                });
                            }
                        }
                        Mode::Normal
                    }
                    Err(e) => {
                        form.error = Some(e);
                        Mode::Spawn(form)
                    }
                },
            },
            Mode::Input(d) => self.key_input(d, key),
            Mode::Confirm(c) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.actions.push(c.action);
                    Mode::Normal
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => Mode::Normal,
                _ => Mode::Confirm(c),
            },
            Mode::Templates(v) => self.key_templates(v, key),
            Mode::Tasks(v) => self.key_tasks(v, key),
            Mode::Review(v) => self.key_review(v, key),
            Mode::Pipelines(v) => self.key_pipelines(v, key),
            Mode::Launch(f) => self.key_launch(f, key),
            Mode::Usage(rows) => match key.code {
                KeyCode::Char('r') => self.usage_view(),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('u') => Mode::Normal,
                _ => Mode::Usage(rows),
            },
        };
        self.rebuild();
    }

    fn usage_view(&mut self) -> Mode {
        let queries: Vec<crate::usage::Query> = self
            .set
            .terminals
            .values()
            .filter(|t| !t.address.contains('/') && agent_name(t) == "claude")
            .map(|t| crate::usage::Query {
                addr: t.address.clone(),
                name: t.labels.get(LABEL_NAME).cloned().unwrap_or_else(|| t.display_name().to_string()),
                cwd: t.cwd.clone(),
                created_ms: t.created_ms,
            })
            .collect();
        self.actions.push(Action::LoadUsage(queries));
        Mode::Usage(None)
    }

    pub fn on_usage(&mut self, rows: Vec<crate::usage::Row>) {
        if let Mode::Usage(r) = &mut self.mode {
            *r = Some(rows);
        }
    }

    /// Take settings and last session's view.
    pub fn apply_config(&mut self, cfg: &crate::config::Config) {
        self.grouping = grouping_from(&cfg.board.grouping);
        self.show_finished = cfg.board.show_finished;
        self.key_map = cfg.key_map().unwrap_or_default();
        if cfg.board.remember {
            let ui = crate::config::load_ui_state();
            if let Some(g) = &ui.grouping {
                self.grouping = grouping_from(g);
            }
            if let Some(f) = ui.show_finished {
                self.show_finished = f;
            }
            self.query = ui.query;
            self.want_select = ui.selected;
        }
    }

    /// What to remember for next time.
    pub fn ui_state(&self) -> crate::config::UiState {
        crate::config::UiState {
            grouping: Some(
                match self.grouping {
                    Grouping::Project => "project",
                    Grouping::Tree => "tree",
                    Grouping::Flat => "flat",
                }
                .into(),
            ),
            show_finished: Some(self.show_finished),
            selected: self.selected.clone(),
            query: self.query.clone(),
        }
    }

    /// Live agents in the list right now, except Hiero itself.
    pub fn broadcast_targets(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item(i) if i.live && self.self_addr.as_deref() != Some(i.addr.as_str()) => Some(i.addr.clone()),
                _ => None,
            })
            .collect()
    }

    /// Order items parent-first with children indented under them.
    fn as_tree(&self, items: Vec<Info>) -> Vec<Info> {
        let present: std::collections::HashSet<String> = items.iter().map(|i| i.addr.clone()).collect();
        let mut children: HashMap<String, Vec<Info>> = HashMap::new();
        let mut roots = Vec::new();
        for i in items {
            match i.parent.clone().filter(|p| present.contains(p) && *p != i.addr) {
                Some(p) => children.entry(p).or_default().push(i),
                None => roots.push(i),
            }
        }
        fn walk(i: Info, depth: u16, children: &mut HashMap<String, Vec<Info>>, out: &mut Vec<Info>) {
            let addr = i.addr.clone();
            out.push(Info { depth, ..i });
            if depth < 8 {
                for c in children.remove(&addr).unwrap_or_default() {
                    walk(c, depth + 1, children, out);
                }
            }
        }
        let mut out = Vec::new();
        for r in roots {
            walk(r, 0, &mut children, &mut out);
        }
        // Anything left (cycles) goes at the end, flat.
        out.extend(children.into_values().flatten());
        out
    }

    // ---- pipelines ------------------------------------------------------------

    /// Runs newest first.
    pub fn run_list(&self) -> Vec<&Run> {
        let mut list: Vec<&Run> = self.tasks.runs.values().collect();
        list.sort_by_key(|r| std::cmp::Reverse(r.started_ms));
        list
    }

    fn pipelines_view(&mut self) -> Mode {
        let (specs, errors) = pipeline::load_all();
        Mode::Pipelines(PipelinesView { specs, errors, selected: 0 })
    }

    fn key_pipelines(&mut self, mut v: PipelinesView, key: KeyEvent) -> Mode {
        let runs: Vec<Run> = self.run_list().into_iter().cloned().collect();
        let n = v.specs.len() + runs.len();
        let on_run = v.selected.checked_sub(v.specs.len()).and_then(|i| runs.get(i));
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('w') => return Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if n > 0 => v.selected = (v.selected + 1).min(n - 1),
            KeyCode::Char('k') | KeyCode::Up => v.selected = v.selected.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('r') => {
                if let Some(spec) = v.specs.get(v.selected) {
                    let folder = self
                        .selected_info()
                        .map(|i| i.dir.clone())
                        .filter(|d| !d.is_empty() && self.selected.as_deref().is_some_and(|a| !a.contains('/')))
                        .unwrap_or_else(|| self.launch_dir.clone());
                    return Mode::Launch(Box::new(LaunchForm {
                        spec: spec.clone(),
                        task: TextInput::default(),
                        folder: TextInput::new(shorten_home(&folder)),
                        field: LaunchField::Task,
                        error: None,
                    }));
                }
                if let Some(root) = on_run.and_then(|r| r.members.first().or(r.history.first().and_then(|h| h.first())))
                    && self.set.get(&root.terminal).is_some()
                {
                    self.want_select = Some(root.terminal.clone());
                    return Mode::Normal;
                }
            }
            KeyCode::Char('e') => {
                if let Some(spec) = v.specs.get(v.selected) {
                    self.actions.push(Action::EditPipeline(pipeline::pipelines_dir().join(format!("{}.toml", spec.name))));
                }
            }
            KeyCode::Char('S') => {
                if let Some(r) = on_run.filter(|r| r.status == RunStatus::Running) {
                    let mut r = r.clone();
                    r.status = RunStatus::Stopped;
                    r.note = "stopped by you; its agents keep running".into();
                    return Mode::Confirm(Confirm {
                        message: format!("Stop pipeline run {}? Its agents keep running. (y/n)", r.id),
                        action: Action::TaskOps(vec![TaskOp::RunPut(Box::new(r))]),
                    });
                }
            }
            KeyCode::Char('A') => {
                if let Some(r) = on_run.filter(|r| r.status == RunStatus::Running && r.driver != self.driver_id) {
                    let mut r = r.clone();
                    r.driver = self.driver_id.clone();
                    self.actions.push(Action::TaskOps(vec![TaskOp::RunPut(Box::new(r))]));
                    self.flash("this board drives the run now");
                }
            }
            KeyCode::Char('D') => {
                if let Some(r) = on_run {
                    if r.status == RunStatus::Running {
                        self.flash_error("stop it first (S)");
                    } else {
                        self.actions.push(Action::TaskOps(vec![TaskOp::RunForget(r.id.clone())]));
                    }
                }
            }
            _ => {}
        }
        Mode::Pipelines(v)
    }

    fn key_launch(&mut self, mut f: Box<LaunchForm>, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return self.pipelines_view(),
            KeyCode::Tab | KeyCode::Down | KeyCode::Up | KeyCode::BackTab => {
                f.field = if f.field == LaunchField::Task { LaunchField::Folder } else { LaunchField::Task };
            }
            KeyCode::Enter if f.field == LaunchField::Task => f.field = LaunchField::Folder,
            KeyCode::Enter => return self.submit_launch(f),
            KeyCode::Char('s') if ctrl => return self.submit_launch(f),
            _ => {
                f.error = None;
                match f.field {
                    LaunchField::Task => f.task.handle(key),
                    LaunchField::Folder => f.folder.handle(key),
                };
            }
        }
        Mode::Launch(f)
    }

    fn submit_launch(&mut self, mut f: Box<LaunchForm>) -> Mode {
        let task = f.task.value().trim().to_string();
        let folder = forms::expand_home(f.folder.value().trim());
        if task.is_empty() {
            f.error = Some("say what the pipeline should do".into());
            return Mode::Launch(f);
        }
        if !folder.is_dir() {
            f.error = Some(format!("no such folder: {}", folder.display()));
            return Mode::Launch(f);
        }
        if f.spec.stages.iter().any(|s| s.worktree) && crate::git::probe(&folder).is_none() {
            f.error = Some("this pipeline uses worktrees; pick a folder inside a git repo".into());
            return Mode::Launch(f);
        }
        self.actions.push(Action::LaunchRun {
            spec: f.spec.clone(),
            task,
            folder: folder.to_string_lossy().into_owned(),
            driver: self.driver_id.clone(),
        });
        self.flash(&format!("launching {}…", f.spec.name));
        Mode::Normal
    }

    /// Re-read pipeline files after an edit.
    pub fn reload_pipelines(&mut self) {
        if let Mode::Pipelines(v) = &mut self.mode {
            let (specs, errors) = pipeline::load_all();
            v.specs = specs;
            v.errors = errors;
            v.selected = v.selected.min(v.specs.len().saturating_sub(1));
        }
    }

    /// Advance the runs this Hiero drives.
    pub fn drive_pipelines(&mut self) {
        let runs: Vec<Run> = self
            .tasks
            .runs
            .values()
            .filter(|r| r.status == RunStatus::Running && r.driver == self.driver_id && !self.starting_runs.contains(&r.id))
            .cloned()
            .collect();
        for run in runs {
            match pipeline::next_step(&run, &self.set) {
                Step::Wait => {}
                Step::Start(stage) => {
                    self.starting_runs.insert(run.id.clone());
                    self.actions.push(Action::StartStage { run: Box::new(run), stage, templates: self.templates.clone() });
                }
                Step::Finish => {
                    let mut r = run.clone();
                    r.status = RunStatus::Done;
                    r.history.push(std::mem::take(&mut r.members));
                    r.note = "all stages finished".into();
                    self.alerts.push(Alert {
                        what: "pipeline".into(),
                        title: "hiero · pipeline".into(),
                        body: format!("{} finished: {}", r.pipeline, r.task),
                    });
                    self.starting_runs.insert(r.id.clone()); // until the doc says done
                    self.actions.push(Action::TaskOps(vec![TaskOp::RunPut(Box::new(r))]));
                }
                Step::Fail(why) => {
                    let mut r = run.clone();
                    r.status = RunStatus::Failed;
                    r.note = why.clone();
                    self.alerts.push(Alert {
                        what: "pipeline".into(),
                        title: "hiero · pipeline failed".into(),
                        body: format!("{}: {why}", r.pipeline),
                    });
                    self.starting_runs.insert(r.id.clone());
                    self.actions.push(Action::TaskOps(vec![TaskOp::RunPut(Box::new(r))]));
                }
            }
        }
    }

    /// Tasks newest first.
    pub fn task_list(&self) -> Vec<&Task> {
        let mut list: Vec<&Task> = self.tasks.tasks.values().collect();
        list.sort_by_key(|t| std::cmp::Reverse(t.created_ms));
        list
    }

    fn task_note(&mut self, terminal: &str, what: String) {
        if let Some(t) = self.tasks.by_terminal(terminal)
            && t.status == TaskStatus::Running
        {
            let id = t.id.clone();
            self.actions.push(Action::TaskOps(vec![TaskOp::Note { id, what }]));
        }
    }

    fn open_review(&mut self, task: &Task) -> Mode {
        if !task.status.has_worktree() {
            self.flash_error(&format!("{} was {} — nothing left to review", task.title, task.status.word()));
            return Mode::Normal;
        }
        self.actions.push(Action::LoadReview(Box::new(task.clone())));
        Mode::Review(Box::new(ReviewView {
            task_id: task.id.clone(),
            data: None,
            error: None,
            loading: true,
            file: 0,
            scroll: 0,
        }))
    }

    fn key_tasks(&mut self, mut v: TasksView, key: KeyEvent) -> Mode {
        let list: Vec<Task> = self.task_list().into_iter().cloned().collect();
        let n = list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('T') => return Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if n > 0 => v.selected = (v.selected + 1).min(n - 1),
            KeyCode::Char('k') | KeyCode::Up => v.selected = v.selected.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('v') => {
                if let Some(t) = list.get(v.selected) {
                    return self.open_review(t);
                }
            }
            KeyCode::Char('g') => {
                // Jump to the task's agent on the board.
                if let Some(addr) = list.get(v.selected).and_then(|t| t.terminal.clone())
                    && self.set.get(&addr).is_some()
                {
                    self.want_select = Some(addr);
                    return Mode::Normal;
                }
                self.flash_error("its agent is gone");
            }
            KeyCode::Char('D') => {
                if let Some(t) = list.get(v.selected) {
                    if t.status.has_worktree() {
                        self.flash_error("open it (enter) and merge or discard first");
                    } else {
                        return Mode::Confirm(Confirm {
                            message: format!("Forget task {} from the list? (y/n)", t.title),
                            action: Action::TaskOps(vec![TaskOp::Forget(t.id.clone())]),
                        });
                    }
                }
            }
            _ => {}
        }
        Mode::Tasks(v)
    }

    fn key_review(&mut self, mut v: Box<ReviewView>, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(task) = self.tasks.tasks.get(&v.task_id).cloned() else {
            return Mode::Normal;
        };
        let nfiles = v.data.as_ref().map(|d| d.files.len()).unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab if nfiles > 0 => {
                v.file = (v.file + 1).min(nfiles - 1);
                v.scroll = 0;
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                v.file = v.file.saturating_sub(1);
                v.scroll = 0;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => v.scroll = v.scroll.saturating_add(20),
            KeyCode::Char('d') if ctrl => v.scroll = v.scroll.saturating_add(20),
            KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(20),
            KeyCode::Char('u') if ctrl => v.scroll = v.scroll.saturating_sub(20),
            KeyCode::Char('r') => {
                v.loading = true;
                self.actions.push(Action::LoadReview(Box::new(task)));
            }
            KeyCode::Char('m') => {
                let live = task.terminal.as_deref().and_then(|a| self.set.get(a)).is_some_and(|t| t.is_live());
                let stop = if live { " The agent is stopped first." } else { "" };
                return Mode::Confirm(Confirm {
                    message: format!(
                        "Merge {} into {}? Uncommitted work is committed, then the worktree and branch are removed.{stop} (y/n)",
                        task.branch, task.base_branch
                    ),
                    action: Action::TaskMerge(Box::new(task)),
                });
            }
            KeyCode::Char('P') => {
                return Mode::Confirm(Confirm {
                    message: format!(
                        "Push {} and open a pull request into {}? Uncommitted work is committed first. (y/n)",
                        task.branch, task.base_branch
                    ),
                    action: Action::TaskPr(Box::new(task)),
                });
            }
            KeyCode::Char('K') => {
                self.actions.push(Action::TaskKeep(Box::new(task)));
                return Mode::Normal;
            }
            KeyCode::Char('D') => {
                return Mode::Confirm(Confirm {
                    message: format!(
                        "Discard {}? The agent is stopped and its worktree and branch are deleted. (y/n)",
                        task.title
                    ),
                    action: Action::TaskDiscard(Box::new(task)),
                });
            }
            _ => {}
        }
        Mode::Review(v)
    }

    pub fn on_review(&mut self, task_id: &str, result: Result<worktree::Review, String>) {
        if let Mode::Review(v) = &mut self.mode
            && v.task_id == task_id
        {
            v.loading = false;
            match result {
                Ok(r) => {
                    v.file = v.file.min(r.files.len().saturating_sub(1));
                    v.data = Some(r);
                    v.error = None;
                }
                Err(e) => v.error = Some(e),
            }
        }
    }

    pub fn on_doc(&mut self, doc: tasks::Doc) {
        self.tasks = doc;
        // Runs no longer running need no guard.
        let runs = &self.tasks.runs;
        self.starting_runs.retain(|id| runs.get(id).is_some_and(|r| r.status == RunStatus::Running));
        self.drive_pipelines();
        self.rebuild();
    }

    /// A stage finished starting (or failed to).
    pub fn on_stage_started(&mut self, run_id: &str) {
        self.starting_runs.remove(run_id);
    }

    /// Pasted text goes into whatever is being typed.
    pub fn on_paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Search => self.query.push_str(text.trim()),
            Mode::Spawn(form) => form.paste(text),
            Mode::Input(d) => d.input.insert_str(text),
            _ => {}
        }
        self.rebuild();
    }

    fn key_search(&mut self, key: KeyEvent) -> Mode {
        match key.code {
            KeyCode::Esc => {
                self.query.clear();
                Mode::Normal
            }
            KeyCode::Enter => Mode::Normal,
            KeyCode::Backspace => {
                self.query.pop();
                Mode::Search
            }
            KeyCode::Down => {
                self.move_by(1);
                Mode::Search
            }
            KeyCode::Up => {
                self.move_by(-1);
                Mode::Search
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.rebuild();
                Mode::Search
            }
            _ => Mode::Search,
        }
    }

    fn key_normal(&mut self, mut key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // `[keys]` remaps: translate to the default key, or swallow a disabled one.
        if let KeyCode::Char(c) = key.code
            && !ctrl
            && let Some(&to) = self.key_map.get(&c)
        {
            if to == '\0' {
                return Mode::Normal;
            }
            key.code = KeyCode::Char(to);
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => return Mode::Help,
            KeyCode::Char('u') if ctrl => self.preview.scroll = self.preview.scroll.saturating_add(10),
            KeyCode::Char('d') if ctrl => self.preview.scroll = self.preview.scroll.saturating_sub(10),
            KeyCode::PageUp => self.preview.scroll = self.preview.scroll.saturating_add(10),
            KeyCode::PageDown => self.preview.scroll = self.preview.scroll.saturating_sub(10),
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_to_edge(false),
            KeyCode::Char('G') | KeyCode::End => self.move_to_edge(true),
            KeyCode::Char('n') | KeyCode::Tab => self.next_attention(),
            KeyCode::Char('/') => return Mode::Search,
            KeyCode::Esc => self.query.clear(),
            KeyCode::Char('p') => {
                self.grouping = match self.grouping {
                    Grouping::Project => Grouping::Tree,
                    Grouping::Tree => Grouping::Flat,
                    Grouping::Flat => Grouping::Project,
                };
                self.flash(match self.grouping {
                    Grouping::Project => "grouped by project",
                    Grouping::Tree => "tree: children under their parents",
                    Grouping::Flat => "one list",
                });
            }
            KeyCode::Char('B') => {
                let addrs = self.broadcast_targets();
                if addrs.is_empty() {
                    self.flash_error("no live agents shown to broadcast to");
                } else {
                    return Mode::Input(InputDialog {
                        title: format!("broadcast to {} agent{} shown", addrs.len(), if addrs.len() == 1 { "" } else { "s" }),
                        hint: "enter sends to every live agent in the list (search narrows it) · esc cancel".into(),
                        input: TextInput::default(),
                        purpose: InputPurpose::Broadcast(addrs),
                        force: false,
                    });
                }
            }
            KeyCode::Char('w') => return self.pipelines_view(),
            KeyCode::Char('u') => return self.usage_view(),
            KeyCode::Char('f') => {
                self.show_finished = !self.show_finished;
                self.flash(if self.show_finished { "showing finished" } else { "hiding finished" });
            }
            KeyCode::Char('x') => self.dismiss_selected(),
            KeyCode::Char('r') => self.preview.dirty = true,

            KeyCode::Char('c') => return self.spawn_form(None),
            KeyCode::Char('T') => {
                self.actions.push(Action::LoadDoc);
                return Mode::Tasks(TasksView { selected: 0 });
            }
            KeyCode::Char('v') => {
                let task = self.selected.as_deref().and_then(|a| self.tasks.by_terminal(a)).cloned();
                match task {
                    Some(t) => return self.open_review(&t),
                    None => self.flash_error("no task here — start one with c and worktree on, or see T"),
                }
            }
            KeyCode::Char('t') => {
                self.actions.push(Action::LoadTemplates);
                return Mode::Templates(TemplatesView {
                    items: self.templates.clone(),
                    selected: 0,
                });
            }
            KeyCode::Char('s') => {
                if let Some((addr, name)) = self.target(true) {
                    return Mode::Input(InputDialog {
                        title: format!("send to {name}"),
                        hint: "enter send · ctrl+f force when blocked · esc cancel".into(),
                        input: TextInput::default(),
                        purpose: InputPurpose::Send(addr),
                        force: false,
                    });
                }
            }
            KeyCode::Char('i') => {
                if let Some((addr, _)) = self.target(true) {
                    self.actions.push(Action::Interrupt(addr));
                }
            }
            KeyCode::Char('K') => {
                if let Some((addr, name)) = self.target(true) {
                    return Mode::Confirm(Confirm {
                        message: format!("Kill {name}? Its program ends. (y/n)"),
                        action: Action::Kill(addr),
                    });
                }
            }
            KeyCode::Char('d') => {
                if let Some((addr, _)) = self.target(false) {
                    if self.set.get(&addr).is_some_and(|t| t.is_live()) {
                        self.flash_error("still running — K kills it first");
                    } else {
                        self.actions.push(Action::Remove(addr));
                    }
                }
            }
            KeyCode::Char('e') => {
                if let Some((addr, _)) = self.target(true) {
                    let keep = self.set.get(&addr).is_some_and(|t| t.keep);
                    self.actions.push(Action::Keep(addr, !keep));
                }
            }
            KeyCode::Char('R') => {
                if let Some((addr, name)) = self.target(false) {
                    return Mode::Input(InputDialog {
                        title: format!("rename {name}"),
                        hint: "enter save · empty restores the original · esc cancel".into(),
                        input: TextInput::new(name),
                        purpose: InputPurpose::Rename(addr),
                        force: false,
                    });
                }
            }
            KeyCode::Char('a') | KeyCode::Enter => {
                if let Some((addr, _)) = self.target(true) {
                    self.actions.push(Action::AttachFull(addr));
                }
            }
            KeyCode::Char('o') => {
                if let Some((addr, _)) = self.target(true) {
                    self.actions.push(Action::AttachPane(addr));
                }
            }
            _ => {}
        }
        Mode::Normal
    }

    fn key_input(&mut self, mut d: InputDialog, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                if matches!(d.purpose, InputPurpose::NewTemplate) {
                    return self.templates_view();
                }
                Mode::Normal
            }
            KeyCode::Char('f') if ctrl && matches!(d.purpose, InputPurpose::Send(_)) => {
                d.force = !d.force;
                Mode::Input(d)
            }
            KeyCode::Enter => {
                let text = d.input.value().trim().to_string();
                match &d.purpose {
                    InputPurpose::Send(addr) => {
                        if text.is_empty() {
                            return Mode::Input(d);
                        }
                        self.actions.push(Action::Send {
                            addr: addr.clone(),
                            text,
                            force: d.force,
                        });
                        Mode::Normal
                    }
                    InputPurpose::Rename(addr) => {
                        // An empty value removes the label, restoring hc's name.
                        let labels = Labels::from([(LABEL_NAME.to_string(), text)]);
                        self.actions.push(Action::Label(addr.clone(), labels));
                        Mode::Normal
                    }
                    InputPurpose::Broadcast(addrs) => {
                        if text.is_empty() {
                            return Mode::Input(d);
                        }
                        self.actions.push(Action::Broadcast { addrs: addrs.clone(), text });
                        Mode::Normal
                    }
                    InputPurpose::NewTemplate => {
                        if !forms::valid_template_name(&text) {
                            self.flash_error("use letters, digits, - and _");
                            return Mode::Input(d);
                        }
                        let path = forms::template_path(&text);
                        if path.exists() {
                            self.flash_error("a template with that name exists");
                            return Mode::Input(d);
                        }
                        self.actions.push(Action::EditTemplate(path));
                        self.templates_view()
                    }
                }
            }
            _ => {
                d.input.handle(key);
                Mode::Input(d)
            }
        }
    }

    fn key_templates(&mut self, mut v: TemplatesView, key: KeyEvent) -> Mode {
        let n = v.items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if n > 0 => v.selected = (v.selected + 1).min(n - 1),
            KeyCode::Char('k') | KeyCode::Up => v.selected = v.selected.saturating_sub(1),
            KeyCode::Char('n') => {
                return Mode::Input(InputDialog {
                    title: "new template".into(),
                    hint: "name (letters, digits, - _) · enter opens your editor · esc back".into(),
                    input: TextInput::default(),
                    purpose: InputPurpose::NewTemplate,
                    force: false,
                });
            }
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(t) = v.current() {
                    self.actions.push(Action::EditTemplate(forms::template_path(&t.name)));
                }
            }
            KeyCode::Char('D') => {
                if let Some(t) = v.current() {
                    return Mode::Confirm(Confirm {
                        message: format!("Delete template {}? (y/n)", t.name),
                        action: Action::DeleteTemplate(forms::template_path(&t.name)),
                    });
                }
            }
            KeyCode::Char('r') | KeyCode::Char('c') => {
                if let Some(name) = v.current().map(|t| t.name.clone()) {
                    return self.spawn_form(Some(&name));
                }
            }
            _ => {}
        }
        Mode::Templates(v)
    }

    fn templates_view(&self) -> Mode {
        Mode::Templates(TemplatesView {
            items: self.templates.clone(),
            selected: 0,
        })
    }

    fn spawn_form(&self, preselect: Option<&str>) -> Mode {
        let folder = self
            .selected_info()
            .map(|i| i.dir.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| self.launch_dir.clone());
        let folder = shorten_home(&folder);
        let mut form = SpawnForm::new(forms::agent_choices(&self.templates), folder, preselect);
        form.machines.extend(self.machines.iter().cloned());
        Mode::Spawn(Box::new(form))
    }

    /// The selected terminal as (address, name), if it fits.
    fn target(&mut self, need_live: bool) -> Option<(String, String)> {
        let Some(info) = self.selected_info() else {
            self.flash_error("nothing selected");
            return None;
        };
        let (addr, name, live) = (info.addr.clone(), info.name.clone(), info.live);
        if need_live && self.self_addr.as_deref() == Some(addr.as_str()) {
            self.flash_error("that's this board's own terminal");
            return None;
        }
        if need_live && !live {
            self.flash_error(&format!("{name} has finished"));
            return None;
        }
        Some((addr, name))
    }

    // ---- results from the event loop ----------------------------------------

    pub fn on_action_done(&mut self, result: Result<String, String>, select: Option<String>) {
        match result {
            Ok(msg) if !msg.is_empty() => self.flash(&msg),
            Ok(_) => {}
            Err(e) => self.flash_error(&e),
        }
        if select.is_some() {
            self.want_select = select;
        }
        self.rebuild();
    }

    pub fn on_templates(&mut self, templates: Vec<TemplateInfo>) {
        self.templates = templates;
        if let Mode::Templates(v) = &mut self.mode {
            v.items = self.templates.clone();
            v.selected = v.selected.min(v.items.len().saturating_sub(1));
        }
    }

    pub fn enter_pane(&mut self, view: PaneView) {
        self.pane = Some(view);
        self.mode = Mode::Pane;
    }

    pub fn leave_pane(&mut self) {
        self.pane = None;
        if matches!(self.mode, Mode::Pane) {
            self.mode = Mode::Normal;
        }
        self.preview.dirty = true;
    }

    pub fn flash(&mut self, msg: &str) {
        self.flash = Some(Flash {
            msg: msg.to_string(),
            at: Instant::now(),
            error: false,
        });
    }

    pub fn flash_error(&mut self, msg: &str) {
        self.flash = Some(Flash {
            msg: msg.to_string(),
            at: Instant::now(),
            error: true,
        });
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
                let key = match self.grouping {
                    Grouping::Project => info.project.clone(),
                    Grouping::Tree | Grouping::Flat => "agents".into(),
                };
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
            if self.grouping == Grouping::Tree {
                items = self.as_tree(items);
            }
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
        if let Some(want) = &self.want_select
            && items.contains(&want.as_str())
        {
            let want = self.want_select.take();
            self.select(want);
            return;
        }
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
        // Remote folders aren't on this disk.
        let git = if t.address.contains('/') { None } else { self.git.lookup(&dir) };
        let project = t
            .labels
            .get(LABEL_PROJECT)
            .cloned()
            .or_else(|| git.as_ref().map(|g| g.name()))
            .unwrap_or_else(|| basename(&dir));

        // Say which machine a remote agent is on.
        let project = match t.address.split_once('/') {
            Some((machine, _)) => format!("{machine}:{project}"),
            None => project,
        };
        let att = attention(t);
        let dismissed = self.dismissed.get(&t.address) == Some(&attention_ts(t));
        let wait_ms = match att {
            Some(_) if !dismissed => Some(now.saturating_sub(attention_ts(t))),
            _ => None,
        };
        let end = t.finished_ms.unwrap_or(now);
        Info {
            addr: t.address.clone(),
            name: t
                .labels
                .get(LABEL_NAME)
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| t.display_name().to_string()),
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
            task: self.tasks.by_terminal(&t.address).map(|k| k.title.clone()),
            depth: 0,
            parent: t.parent.clone(),
        }
    }
}

// ---- helpers ----------------------------------------------------------------

fn grouping_from(word: &str) -> Grouping {
    match word {
        "tree" => Grouping::Tree,
        "flat" => Grouping::Flat,
        _ => Grouping::Project,
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The hook's fallback (labels) when an agent couldn't report through its
/// terminal. A real OSC 7501 report always wins.
fn status_from_labels(t: &mut Terminal) {
    use crate::integrate::{LABEL_STATUS, LABEL_STATUS_KIND, LABEL_STATUS_MS, LABEL_STATUS_MSG};
    if t.reported || !t.is_live() {
        return;
    }
    let Some(state) = t.labels.get(LABEL_STATUS).filter(|s| !s.is_empty()) else {
        return;
    };
    let get = |k: &str| t.labels.get(k).cloned().unwrap_or_default();
    let kind = get(LABEL_STATUS_KIND);
    t.status = Some(hc_client::Status {
        state: state.clone(),
        app: t.labels.get(LABEL_AGENT).cloned().unwrap_or_default(),
        kind: (!kind.is_empty()).then_some(kind),
        msg: get(LABEL_STATUS_MSG),
        progress: None,
        title: String::new(),
    });
    t.status_ms = get(LABEL_STATUS_MS).parse().ok().or(t.status_ms);
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
    // Pipeline members are meant to finish; only a failure needs a person.
    if t.labels.contains_key("hiero.run") && t.exit_code == Some(0) {
        return None;
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
        what: a.word().to_string(),
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
        return match app {
            "claude-code" => "claude".to_string(),
            other => other.to_string(),
        };
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

pub fn shorten_home(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
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
