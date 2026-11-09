//! The board's event loop: keys, core events, screen reads, actions,
//! attached agents and timers.

use crate::app::{Action, App, Mode, PaneView};
use crate::pipeline;
use crate::tasks::{self, TaskOp, TaskStatus};
use crate::{attach, forms, keys, notify, ui, worktree};
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyEventKind,
};
use crossterm::{execute, terminal};
use futures::StreamExt;
use hc_client::{
    AttachParams, AttachStream, Client, ErrorKind, Filter, StreamMsg, TemplateInfo, WatchMsg, Watcher, endpoint,
};
use ratatui::layout::Rect;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

/// History lines to fetch above the visible screen for the preview.
const PREVIEW_HISTORY: usize = 200;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Results coming back from spawned work.
enum Msg {
    Connected(Result<(Client, String), String>),
    Preview { addr: String, result: Result<String, String>, lost: bool },
    Done { result: Result<String, String>, select: Option<String> },
    Templates(Vec<TemplateInfo>),
    Doc(tasks::Doc),
    Machines(Vec<String>),
    StageStarted(String),
    Review { id: String, result: Result<worktree::Review, String> },
    PaneOpened { addr: String, name: String, cols: u16, rows: u16, result: Result<Box<AttachStream>, String> },
}

pub async fn run() -> Result<()> {
    let path = endpoint::socket_path();
    let mut watch = Watcher::spawn(path.clone(), Filter::default());
    let mut term = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let result = event_loop(&mut term, &mut watch, path).await;
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    watch: &mut mpsc::Receiver<WatchMsg>,
    path: PathBuf,
) -> Result<()> {
    let mut app = App::new();
    let mut keys = EventStream::new();

    // A request connection (the watcher's is events-only).
    let mut client: Option<Client> = None;
    let mut connecting = false;
    let mut next_connect = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // The agent attached inside the board, if any.
    let mut pane: Option<AttachStream> = None;

    let mut anim = interval(Duration::from_millis(120));
    let mut slow = interval(Duration::from_secs(1));

    loop {
        if client.is_none() && !connecting && app.disconnected.is_none() && Instant::now() >= next_connect {
            connecting = true;
            let (tx, path) = (tx.clone(), path.clone());
            tokio::spawn(async move {
                let r = async {
                    let c = Client::connect_to(&path).await?;
                    let name = c.hello().await?.core.name;
                    Ok::<_, hc_client::Error>((c, name))
                }
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(Msg::Connected(r));
            });
        }
        if let Some(c) = &client
            && pane.is_none()
            && let Some(addr) = app.take_preview_request()
        {
            let (c, tx) = (c.clone(), tx.clone());
            tokio::spawn(async move {
                let (result, lost) = match c.read(&addr, PREVIEW_HISTORY).await {
                    Ok(r) => (Ok(r.text), false),
                    Err(e) => {
                        let lost = matches!(e, hc_client::Error::Disconnected | hc_client::Error::Io(_));
                        (Err(e.to_string()), lost)
                    }
                };
                let _ = tx.send(Msg::Preview { addr, result, lost });
            });
        }

        // Work the keys asked for.
        for action in std::mem::take(&mut app.actions) {
            match action {
                Action::AttachFull(addr) => {
                    if let Some(p) = pane.take() {
                        p.close().await;
                        app.leave_pane();
                    }
                    drop(keys);
                    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
                    ratatui::restore();
                    let outcome = attach::fullscreen(&path, &addr).await;
                    resume(term)?;
                    keys = EventStream::new();
                    match outcome {
                        Ok(attach::Outcome::Detached) => app.flash("back on the board"),
                        Ok(attach::Outcome::Ended(why)) => app.flash(&format!("{addr} ended: {why}")),
                        Err(e) => app.flash_error(&format!("attach failed: {e}")),
                    }
                    app.preview.dirty = true;
                }
                Action::EditTemplate(file) => {
                    drop(keys);
                    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
                    ratatui::restore();
                    let r = edit_file(&file);
                    resume(term)?;
                    keys = EventStream::new();
                    if let Err(e) = r {
                        app.flash_error(&e);
                    }
                    if let Some(c) = &client {
                        load_templates(c.clone(), tx.clone());
                    }
                }
                Action::EditPipeline(file) => {
                    drop(keys);
                    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
                    ratatui::restore();
                    let r = edit_pipeline(&file);
                    resume(term)?;
                    keys = EventStream::new();
                    if let Err(e) = r {
                        app.flash_error(&e);
                    }
                    app.reload_pipelines();
                }
                Action::AttachPane(addr) => {
                    let name = app.selected_info().map(|i| i.name.clone()).unwrap_or_else(|| addr.clone());
                    let inner = pane_size(term, app.disconnected.is_some())?;
                    let (tx, path) = (tx.clone(), path.clone());
                    tokio::spawn(async move {
                        let params = AttachParams::new(addr.clone(), inner.width, inner.height);
                        let result = AttachStream::open_at(&path, &params).await.map(Box::new).map_err(|e| e.to_string());
                        let _ = tx.send(Msg::PaneOpened { addr, name, cols: inner.width, rows: inner.height, result });
                    });
                }
                other => match &client {
                    Some(c) => run_action(c.clone(), other, tx.clone()),
                    None => app.flash_error("not connected to herdr-core"),
                },
            }
        }

        term.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(k))) if k.kind != KeyEventKind::Release => {
                    if matches!(app.mode, Mode::Pane) {
                        let ended = app.pane.as_ref().is_some_and(|p| p.ended.is_some());
                        if keys::is_detach(&k) || ended {
                            if let Some(p) = pane.take() {
                                tokio::spawn(p.close());
                            }
                            app.leave_pane();
                        } else if let (Some(p), Some(view)) = (&pane, &app.pane) {
                            let app_cursor = view.parser.screen().application_cursor();
                            if let Some(bytes) = keys::encode(k, app_cursor) {
                                let _ = p.input(&bytes).await;
                            }
                        }
                    } else if k.kind == KeyEventKind::Press {
                        app.on_key(k);
                    }
                }
                Some(Ok(TermEvent::Paste(text))) => {
                    if let (Mode::Pane, Some(p), Some(view)) = (&app.mode, &pane, &app.pane) {
                        let bytes = if view.parser.screen().bracketed_paste() {
                            format!("\x1b[200~{text}\x1b[201~")
                        } else {
                            text
                        };
                        let _ = p.input(bytes.as_bytes()).await;
                    } else {
                        app.on_paste(&text);
                    }
                }
                Some(Ok(TermEvent::Resize(_, _))) => {
                    if let (Some(p), Some(view)) = (&pane, &mut app.pane) {
                        let inner = pane_size(term, app.disconnected.is_some())?;
                        view.resize(inner.width, inner.height);
                        let _ = p.resize(inner.width, inner.height).await;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            msg = watch.recv() => {
                let Some(msg) = msg else { break };
                if matches!(msg, WatchMsg::Disconnected { .. }) {
                    client = None;
                }
                for a in app.on_watch(&msg) {
                    notify::alert(&a.title, &a.body);
                }
            }
            msg = pane_recv(&mut pane) => {
                if let Some(view) = &mut app.pane {
                    match msg {
                        StreamMsg::Data(bytes) => view.parser.process(&bytes),
                        StreamMsg::Resize { .. } => {}
                        StreamMsg::Closed(reason) => {
                            view.ended = Some(attach::describe_close(reason.as_ref()));
                            pane = None;
                        }
                    }
                }
            }
            Some(msg) = rx.recv() => match msg {
                Msg::Connected(Ok((c, name))) => {
                    connecting = false;
                    load_templates(c.clone(), tx.clone());
                    load_doc(c.clone(), tx.clone());
                    run_action(c.clone(), Action::LoadMachines, tx.clone());
                    client = Some(c);
                    app.core_name = name;
                    app.preview.dirty = true;
                }
                Msg::Connected(Err(e)) => {
                    connecting = false;
                    tracing::debug!("request connection failed: {e}");
                    next_connect = Instant::now() + RECONNECT_DELAY;
                }
                Msg::Preview { addr, result, lost } => {
                    if lost {
                        client = None;
                        next_connect = Instant::now() + RECONNECT_DELAY;
                    }
                    app.on_preview(addr, result);
                }
                Msg::Done { result, select } => {
                    let reload = result.as_ref().is_ok_and(|m| m.starts_with("deleted template"));
                    app.on_action_done(result, select);
                    if reload && let Some(c) = &client {
                        load_templates(c.clone(), tx.clone());
                    }
                }
                Msg::Templates(t) => app.on_templates(t),
                Msg::Doc(doc) => app.on_doc(doc),
                Msg::Machines(m) => app.machines = m,
                Msg::StageStarted(id) => {
                    app.on_stage_started(&id);
                    app.drive_pipelines();
                }
                Msg::Review { id, result } => app.on_review(&id, result),
                Msg::PaneOpened { addr, name, cols, rows, result } => match result {
                    Ok(stream) => {
                        app.enter_pane(PaneView::new(addr, name, cols, rows));
                        let _ = stream.focus(true).await;
                        pane = Some(*stream);
                    }
                    Err(e) => app.flash_error(&format!("attach failed: {e}")),
                },
            },
            _ = anim.tick() => app.tick = app.tick.wrapping_add(1),
            _ = slow.tick() => {
                app.on_slow_tick();
                app.rebuild();
            }
        }
        if app.quit {
            break;
        }
    }
    if let Some(p) = pane.take() {
        p.close().await;
    }
    Ok(())
}

async fn pane_recv(pane: &mut Option<AttachStream>) -> StreamMsg {
    match pane {
        Some(p) => p.recv().await,
        None => std::future::pending().await,
    }
}

fn pane_size(term: &ratatui::DefaultTerminal, banner: bool) -> Result<Rect> {
    let size = term.size()?;
    let inner = ui::pane_inner(Rect::new(0, 0, size.width, size.height), banner);
    Ok(Rect::new(0, 0, inner.width.max(1), inner.height.max(1)))
}

/// Come back to the board after handing the terminal to something else.
///
/// Uses a fresh terminal handle instead of `Terminal::clear`, which asks the
/// terminal for the cursor position and fails if that reply gets lost.
fn resume(term: &mut ratatui::DefaultTerminal) -> Result<()> {
    terminal::enable_raw_mode()?;
    execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        terminal::Clear(terminal::ClearType::All)
    )?;
    *term = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    Ok(())
}

/// Open a pipeline file in the editor; a new one starts as a copy of the built-in.
fn edit_pipeline(file: &Path) -> Result<(), String> {
    if !file.exists() {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
        }
        std::fs::write(file, pipeline::BUILTIN).map_err(|e| format!("can't write pipeline: {e}"))?;
    }
    open_editor(file)
}

fn open_editor(file: &Path) -> Result<(), String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let mut argv = shell_words::split(&editor).map_err(|e| format!("bad $EDITOR: {e}"))?;
    if argv.is_empty() {
        argv.push("vi".into());
    }
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .arg(file)
        .status()
        .map_err(|e| format!("can't start {}: {e}", argv[0]))?;
    if !status.success() {
        return Err(format!("{} exited with {status}", argv[0]));
    }
    Ok(())
}

/// Open a template in the person's editor, creating it from a skeleton first.
fn edit_file(file: &Path) -> Result<(), String> {
    if !file.exists() {
        let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
        }
        std::fs::write(file, forms::template_skeleton(&name)).map_err(|e| format!("can't write template: {e}"))?;
    }
    open_editor(file)
}

fn load_templates(c: Client, tx: mpsc::UnboundedSender<Msg>) {
    tokio::spawn(async move {
        match c.templates().await {
            Ok(t) => {
                let _ = tx.send(Msg::Templates(t.templates));
            }
            Err(e) => tracing::debug!("templates: {e}"),
        }
    });
}

fn load_doc(c: Client, tx: mpsc::UnboundedSender<Msg>) {
    tokio::spawn(async move {
        match tasks::load(&c).await {
            Ok(doc) => {
                let _ = tx.send(Msg::Doc(doc));
            }
            Err(e) => tracing::debug!("doc: {e}"),
        }
    });
}

/// Run blocking git work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("git task failed: {e}"))?
}

/// Task actions: worktrees, records, merge / PR / keep / discard.
async fn run_task_action(c: Client, action: Action, tx: mpsc::UnboundedSender<Msg>) {
    let mut select = None;
    let result: Result<String, String> = async {
        match action {
            Action::RunTask { params, send_after, spec } => {
                let (t, task) = start_task(&c, params, send_after, spec, &tx).await?;
                select = Some(t.address.clone());
                Ok(format!("started {} on {} in {}", t.display_name(), task.branch, crate::app::shorten_home(&task.worktree)))
            }
            Action::LaunchRun { spec, task, folder, driver } => {
                let now = crate::app::now_ms();
                let run = pipeline::Run {
                    id: format!("{}-{:x}", worktree::slugify(&spec.name), now % 0xff_ffff),
                    pipeline: spec.name.clone(),
                    task,
                    folder,
                    stages: spec.stages.clone(),
                    stage: 0,
                    members: vec![],
                    history: vec![],
                    status: pipeline::RunStatus::Running,
                    driver,
                    started_ms: now,
                    note: String::new(),
                };
                let doc = tasks::update(&c, &[TaskOp::RunPut(Box::new(run))]).await?;
                let _ = tx.send(Msg::Doc(doc));
                Ok(String::new())
            }
            Action::TaskOps(ops) => {
                let doc = tasks::update(&c, &ops).await?;
                let _ = tx.send(Msg::Doc(doc));
                Ok(String::new())
            }
            Action::LoadReview(task) => {
                let (wt, base) = (task.worktree.clone(), task.base_branch.clone());
                let result = blocking(move || worktree::review(std::path::Path::new(&wt), &base)).await;
                let _ = tx.send(Msg::Review { id: task.id.clone(), result });
                Ok(String::new())
            }
            Action::TaskKeep(task) => {
                let doc = tasks::update(&c, &[TaskOp::Status { id: task.id.clone(), status: TaskStatus::Kept, note: String::new() }]).await?;
                let _ = tx.send(Msg::Doc(doc));
                Ok(format!("kept {} in {}", task.branch, crate::app::shorten_home(&task.worktree)))
            }
            Action::TaskMerge(task) => {
                stop_agent(&c, &task).await;
                let t = task.clone();
                blocking(move || {
                    let (wt, repo) = (std::path::Path::new(&t.worktree), std::path::Path::new(&t.repo));
                    worktree::commit_all(wt, &format!("hiero: {}", t.title))?;
                    worktree::merge(repo, &t.base_branch, &t.branch, &format!("Merge task: {}", t.title))?;
                    worktree::remove(repo, wt, &t.branch)
                })
                .await?;
                let note = format!("merged into {}", task.base_branch);
                let doc = tasks::update(&c, &[TaskOp::Status { id: task.id.clone(), status: TaskStatus::Merged, note }]).await?;
                let _ = tx.send(Msg::Doc(doc));
                Ok(format!("merged {} into {} and cleaned up", task.branch, task.base_branch))
            }
            Action::TaskPr(task) => {
                let t = task.clone();
                let url = blocking(move || {
                    let (wt, repo) = (std::path::Path::new(&t.worktree), std::path::Path::new(&t.repo));
                    worktree::commit_all(wt, &format!("hiero: {}", t.title))?;
                    let body = if t.prompt.is_empty() { "Opened from Hiero.".to_string() } else { t.prompt.clone() };
                    worktree::open_pr(wt, repo, &t.base_branch, &t.branch, &t.title, &body)
                })
                .await?;
                let doc = tasks::update(
                    &c,
                    &[
                        TaskOp::SetPr { id: task.id.clone(), url: url.clone() },
                        TaskOp::Status { id: task.id.clone(), status: TaskStatus::Pr, note: format!("pull request {url}") },
                    ],
                )
                .await?;
                let _ = tx.send(Msg::Doc(doc));
                Ok(format!("pull request: {url}"))
            }
            Action::TaskDiscard(task) => {
                stop_agent(&c, &task).await;
                let t = task.clone();
                blocking(move || worktree::remove(std::path::Path::new(&t.repo), std::path::Path::new(&t.worktree), &t.branch)).await?;
                let doc = tasks::update(&c, &[TaskOp::Status { id: task.id.clone(), status: TaskStatus::Discarded, note: String::new() }]).await?;
                let _ = tx.send(Msg::Doc(doc));
                Ok(format!("discarded {}", task.title))
            }
            _ => Ok(String::new()),
        }
    }
    .await;
    if result.as_ref().is_ok_and(|m| m.is_empty()) && select.is_none() {
        return;
    }
    let _ = tx.send(Msg::Done { result, select });
}

/// Make a worktree, start the agent in it, and record the task.
async fn start_task(
    c: &Client,
    mut params: hc_client::RunParams,
    send_after: Option<String>,
    spec: forms::TaskSpec,
    tx: &mpsc::UnboundedSender<Msg>,
) -> Result<(hc_client::Terminal, tasks::Task), String> {
    let folder = spec.folder.clone();
    let slug = spec.slug.clone();
    let made = blocking(move || worktree::create(&folder, &slug)).await?;
    let now = crate::app::now_ms();
    let id = tasks::new_id(&made.slug, now);
    params.cwd = Some(made.path.to_string_lossy().into_owned());
    params.labels.insert(crate::app::LABEL_PROJECT.into(), worktree_project(&made.repo));
    params.labels.insert("hiero.task_id".into(), id.clone());
    if params.name.is_none() {
        params.name = Some(made.slug.clone());
    }
    let started = async {
        let t = c.run(&params).await.map_err(explain)?;
        if let Some(text) = &send_after {
            c.send(&t.address, text).await.map_err(explain)?;
        }
        Ok::<_, String>(t)
    }
    .await;
    let t = match started {
        Ok(t) => t,
        Err(e) => {
            // Don't leave an empty worktree behind.
            let (repo, path, branch) = (made.repo.clone(), made.path.clone(), made.branch.clone());
            let _ = blocking(move || worktree::remove(&repo, &path, &branch)).await;
            return Err(e);
        }
    };
    let task = tasks::Task {
        id,
        title: spec.title,
        prompt: spec.prompt,
        agent: spec.agent,
        repo: made.repo.to_string_lossy().into_owned(),
        base_branch: made.base_branch,
        base_commit: made.base_commit,
        branch: made.branch.clone(),
        worktree: made.path.to_string_lossy().into_owned(),
        terminal: Some(t.address.clone()),
        created_ms: now,
        status: TaskStatus::Running,
        timeline: vec![],
        pr_url: None,
    };
    let doc = tasks::update(c, &[TaskOp::Insert(Box::new(task.clone()))]).await?;
    let _ = tx.send(Msg::Doc(doc));
    Ok((t, task))
}

/// Start every member of one pipeline stage, then record them on the run.
async fn start_stage(
    c: &Client,
    mut run: pipeline::Run,
    stage: usize,
    templates: &[TemplateInfo],
    tx: &mpsc::UnboundedSender<Msg>,
) -> Result<String, String> {
    let spec = run.stages.get(stage).cloned().ok_or("no such stage")?;
    // The finished stage moves to history first, so {branches}/{dirs} see it.
    if stage != run.stage || !run.members.is_empty() {
        run.history.push(std::mem::take(&mut run.members));
    }
    run.stage = stage;
    let count = spec.members();
    let short = run.id.rsplit('-').next().unwrap_or(&run.id).to_string();
    let parent = run.root().map(str::to_string);
    let project = crate::git::probe(std::path::Path::new(&run.folder))
        .map(|g| g.name())
        .unwrap_or_else(|| worktree_project(std::path::Path::new(&run.folder)));
    let mut members = Vec::new();
    let mut failure = None;
    for n in 1..=count {
        let name = if count == 1 { format!("{}-{short}", spec.name) } else { format!("{}{n}-{short}", spec.name) };
        let mut params = hc_client::RunParams::new()
            .cwd(run.folder.clone())
            .name(name)
            .label("hiero.run", run.id.clone())
            .label("hiero.stage", spec.name.clone())
            .label(crate::app::LABEL_PROJECT, project.clone());
        if let Some(p) = &parent {
            params = params.child_of(p.clone());
        }
        let result: Result<pipeline::Member, String> = async {
            if let Some(cmd) = &spec.command {
                params = params.line(pipeline::fill(cmd, &run, n, count)).label(crate::app::LABEL_AGENT, "command");
                let t = c.run(&params).await.map_err(explain)?;
                return Ok(pipeline::Member { terminal: t.address, task_id: None, branch: None, dir: run.folder.clone(), start_turn: 0, command: true });
            }
            let agent = spec.agent.clone().unwrap_or_else(|| "claude".into());
            let kind = pipeline::resolve_agent(&agent, templates);
            let prompt = pipeline::fill(spec.prompt.as_deref().unwrap_or(""), &run, n, count);
            let (argv, send_after) = forms::agent_argv(&kind, &prompt)?;
            params = params.argv(argv).label(crate::app::LABEL_AGENT, agent.clone()).label(crate::app::LABEL_TASK, prompt.chars().take(200).collect::<String>());
            if spec.worktree {
                let task_spec = forms::TaskSpec {
                    slug: worktree::slugify(&format!("{} {} {n} {short}", run.pipeline, spec.name)),
                    title: format!("{} {} {n}/{count}: {}", run.pipeline, spec.name, run.task.chars().take(50).collect::<String>()),
                    prompt: prompt.clone(),
                    agent: agent.clone(),
                    folder: run.folder.clone().into(),
                };
                let (t, task) = start_task(c, params, send_after, task_spec, tx).await?;
                Ok(pipeline::Member { terminal: t.address, task_id: Some(task.id), branch: Some(task.branch), dir: task.worktree, start_turn: t.turn, command: false })
            } else {
                let t = c.run(&params).await.map_err(explain)?;
                if let Some(text) = send_after {
                    c.send(&t.address, &text).await.map_err(explain)?;
                }
                Ok(pipeline::Member { terminal: t.address, task_id: None, branch: None, dir: run.folder.clone(), start_turn: t.turn, command: false })
            }
        }
        .await;
        match result {
            Ok(m) => members.push(m),
            Err(e) => {
                failure = Some(format!("{} {n}: {e}", spec.name));
                break;
            }
        }
    }
    run.members = members;
    if let Some(why) = &failure {
        run.status = pipeline::RunStatus::Failed;
        run.note = why.clone();
    }
    let doc = tasks::update(c, &[TaskOp::RunPut(Box::new(run.clone()))]).await?;
    let _ = tx.send(Msg::Doc(doc));
    match failure {
        Some(why) => Err(format!("pipeline {}: {why}", run.pipeline)),
        None => Ok(format!(
            "{}: stage {}/{} {} started ({} member{})",
            run.pipeline,
            stage + 1,
            run.stages.len(),
            spec.name,
            count,
            if count == 1 { "" } else { "s" }
        )),
    }
}

/// Kill the task's agent (if it still runs) and remove its finished record.
async fn stop_agent(c: &Client, task: &tasks::Task) {
    if let Some(addr) = &task.terminal {
        if c.get(addr).await.is_ok_and(|t| t.is_live()) {
            let _ = c.kill(addr).await;
        }
        let _ = c.rm(addr).await;
    }
}

fn worktree_project(repo: &Path) -> String {
    repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn explain(e: hc_client::Error) -> String {
    Explain::from(e).0
}

/// Run one request-style action and report back.
fn run_action(c: Client, action: Action, tx: mpsc::UnboundedSender<Msg>) {
    match action {
        Action::LoadTemplates => return load_templates(c, tx),
        Action::LoadDoc => return load_doc(c, tx),
        Action::LoadMachines => {
            tokio::spawn(async move {
                if let Ok(m) = c.machines().await {
                    let _ = tx.send(Msg::Machines(m.machines.into_iter().map(|m| m.name).collect()));
                }
            });
            return;
        }
        Action::StartStage { run, stage, templates } => {
            tokio::spawn(async move {
                let id = run.id.clone();
                let result = start_stage(&c, *run, stage, &templates, &tx).await;
                let _ = tx.send(Msg::StageStarted(id));
                let _ = tx.send(Msg::Done { result, select: None });
            });
            return;
        }
        Action::RunTask { .. }
        | Action::LaunchRun { .. }
        | Action::TaskOps(_)
        | Action::LoadReview(_)
        | Action::TaskMerge(_)
        | Action::TaskPr(_)
        | Action::TaskKeep(_)
        | Action::TaskDiscard(_) => {
            tokio::spawn(run_task_action(c, action, tx));
            return;
        }
        _ => {}
    }
    tokio::spawn(async move {
        let mut select = None;
        let result: Result<String, String> = async {
            let msg = match action {
                Action::Run { params, send_after } => {
                    let t = c.run(&params).await?;
                    if let Some(text) = send_after {
                        c.send(&t.address, &text).await?;
                    }
                    select = Some(t.address.clone());
                    format!("started {} ({})", t.display_name(), t.address)
                }
                Action::Send { addr, text, force } => {
                    c.send_with(&addr, &text, force).await?;
                    format!("sent to {addr}")
                }
                Action::Interrupt(addr) => {
                    let r = c.interrupt(&addr).await?;
                    format!("interrupt ({}): {}", r.key, r.outcome)
                }
                Action::Kill(addr) => {
                    c.kill(&addr).await?;
                    format!("killed {addr}")
                }
                Action::Remove(addr) => {
                    c.rm(&addr).await?;
                    format!("removed {addr}")
                }
                Action::Keep(addr, keep) => {
                    c.keep(&addr, keep).await?;
                    format!("keep {} for {addr}", if keep { "on" } else { "off" })
                }
                Action::Broadcast { addrs, text } => {
                    let mut ok = 0;
                    let mut failed = Vec::new();
                    for a in &addrs {
                        match c.send(a, &text).await {
                            Ok(_) => ok += 1,
                            Err(e) => failed.push(format!("{a}: {}", Explain::from(e).0)),
                        }
                    }
                    if failed.is_empty() {
                        format!("sent to {ok} agent{}", if ok == 1 { "" } else { "s" })
                    } else {
                        return Err(Explain(format!("sent to {ok}; failed {}", failed.join(", "))));
                    }
                }
                Action::Label(addr, labels) => {
                    c.label(&addr, &labels).await?;
                    "saved".to_string()
                }
                Action::DeleteTemplate(file) => {
                    std::fs::remove_file(&file).map_err(|e| Explain(format!("can't delete: {e}")))?;
                    format!(
                        "deleted template {}",
                        file.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default()
                    )
                }
                _ => String::new(),
            };
            Ok::<_, Explain>(msg)
        }
        .await
        .map_err(|e| e.0);
        let _ = tx.send(Msg::Done { result, select });
    });
}

/// An error worded for the person.
struct Explain(String);

impl From<hc_client::Error> for Explain {
    fn from(e: hc_client::Error) -> Self {
        let msg = match e.kind() {
            Some(ErrorKind::Blocked) => {
                "the agent is blocked waiting for you — attach (a) to answer, or ctrl+f in send to force".to_string()
            }
            Some(ErrorKind::NameTaken) => "that name is taken — pick another".to_string(),
            _ => e.to_string(),
        };
        Explain(msg)
    }
}
