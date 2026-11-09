//! The workspace screen's event loop: keys, core events, the attached panes.
//! Same shape as the board's loop (`tui`), without the board's state.

use crate::workspace::{self, WsDoc, WsOp};
use crate::ws_app::{Action, Out, Plan, WsApp};
use crate::{config, keys, notify, tui, ws_ui};
use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyEventKind};
use crossterm::execute;
use futures::StreamExt;
use hc_client::{AttachParams, AttachStream, Client, ErrorKind, Filter, StreamMsg, WatchMsg, Watcher, endpoint};
use ratatui::layout::Rect;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

enum Msg {
    Connected(Result<(Client, String), String>),
    Doc(Result<WsDoc, String>),
    /// A change we made; an error is shown, the doc is taken as the new truth.
    Updated(Result<WsDoc, String>),
    /// A background start finished (its terminal is recorded in the doc first).
    Started { pane: String, result: Result<String, String> },
    Opened { pane: String, attempt: u64, addr: String, cols: u16, rows: u16, result: Result<Box<AttachStream>, String> },
    /// Output or the end of a pane's stream, tagged with the attempt it belongs to.
    Stream { pane: String, attempt: u64, msg: StreamMsg },
}

/// What the event loop asks a pane's task to do with its stream.
enum PaneCmd {
    Input(Vec<u8>),
    Resize(u16, u16),
    Focus(bool),
    /// Detach; the program keeps running.
    Close,
}

/// The loop's side of a pane's task.
struct Handle {
    attempt: u64,
    cmd: mpsc::UnboundedSender<PaneCmd>,
}

/// One task per attached pane owns its stream (`recv` needs `&mut`) and
/// waits on both the stream and the loop's commands. Everything it hears is
/// forwarded to the loop tagged with the pane and attempt. It ends when the
/// stream closes or the loop says so.
async fn pane_task(
    pane: String,
    attempt: u64,
    mut stream: AttachStream,
    mut cmds: mpsc::UnboundedReceiver<PaneCmd>,
    tx: mpsc::UnboundedSender<Msg>,
) {
    loop {
        tokio::select! {
            msg = stream.recv() => {
                let closed = matches!(msg, StreamMsg::Closed(_));
                if tx.send(Msg::Stream { pane: pane.clone(), attempt, msg }).is_err() || closed {
                    return;
                }
            }
            cmd = cmds.recv() => match cmd {
                Some(PaneCmd::Input(b)) => { let _ = stream.input(&b).await; }
                Some(PaneCmd::Resize(c, r)) => { let _ = stream.resize(c, r).await; }
                Some(PaneCmd::Focus(g)) => { let _ = stream.focus(g).await; }
                Some(PaneCmd::Close) | None => {
                    stream.close().await;
                    return;
                }
            },
        }
    }
}

pub async fn run(config_error: Option<String>) -> Result<()> {
    let path = endpoint::socket_path();
    let mut watch = Watcher::spawn(path.clone(), Filter::default());
    let mut term = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let result = event_loop(&mut term, &mut watch, path, config_error).await;
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    watch: &mut mpsc::Receiver<WatchMsg>,
    path: PathBuf,
    config_error: Option<String>,
) -> Result<()> {
    let remember = config::get().board.remember;
    let saved = if remember { config::load_ui_state() } else { config::UiState::default() };
    let mut app = WsApp::new(config::get().prefix(), saved.workspace);
    app.set_sidebar(saved.sidebar_width, saved.sidebar_hidden);
    app.auto_restore = config::get().workspace.restore;
    app.self_addr = std::env::var("HC_TERMINAL_ID").ok().filter(|s| !s.is_empty());
    if let Some(e) = config_error {
        tracing::warn!("{e}");
        app.flash_error(&format!("config ignored: {e}"));
    }
    let mut keys = EventStream::new();

    let mut client: Option<Client> = None;
    let mut connecting = false;
    let mut next_connect = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    // The task of each attached pane, by pane id.
    let mut handles: HashMap<String, Handle> = HashMap::new();
    // The pane that last got the focus event.
    let mut last_focus: Option<String> = None;
    let mut slow = interval(Duration::from_secs(1));

    loop {
        if client.is_none() && !connecting && Instant::now() >= next_connect {
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

        // Every pass recomputes where the panes go, so a resize, a layout
        // change or the banner all end up as RESIZE of the streams.
        let size = term.size()?;
        app.set_screen(Rect::new(0, 0, size.width, size.height));

        for action in std::mem::take(&mut app.actions) {
            match action {
                Action::Detach(pane) => {
                    if let Some(h) = handles.remove(&pane) {
                        let _ = h.cmd.send(PaneCmd::Close);
                    }
                }
                Action::Resize { pane, cols, rows } => {
                    if let Some(h) = handles.get(&pane) {
                        let _ = h.cmd.send(PaneCmd::Resize(cols, rows));
                    }
                }
                Action::LoadDoc => {
                    if let Some(c) = &client {
                        let (c, tx) = (c.clone(), tx.clone());
                        tokio::spawn(async move {
                            let _ = tx.send(Msg::Doc(workspace::load(&c).await));
                        });
                    }
                }
                Action::Update(ops) => match &client {
                    Some(c) => {
                        let (c, tx) = (c.clone(), tx.clone());
                        tokio::spawn(async move {
                            let _ = tx.send(Msg::Updated(workspace::update(&c, &ops).await));
                        });
                    }
                    None => app.flash_error("not connected to herdr-core"),
                },
                Action::Kill(addrs) => {
                    if let Some(c) = &client {
                        let c = c.clone();
                        tokio::spawn(async move {
                            for a in addrs {
                                kill_and_remove(&c, &a).await;
                            }
                        });
                    }
                }
                Action::Start { pane, params } => match &client {
                    Some(c) => {
                        tokio::spawn(start_pane(c.clone(), pane, params, tx.clone()));
                    }
                    None => app.on_started(&pane, Err("not connected to herdr-core".into())),
                },
                Action::Board(start) => {
                    // Let go of the screen and the streams (the programs keep
                    // running), run the board here, then take everything back.
                    for (_, h) in handles.drain() {
                        let _ = h.cmd.send(PaneCmd::Close);
                    }
                    last_focus = None;
                    app.suspend();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    drop(keys);
                    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
                    ratatui::restore();
                    let r = tui::run_board(start, None).await;
                    tui::resume(term)?;
                    keys = EventStream::new();
                    // Events from before were not read; a new snapshot is a clean baseline.
                    *watch = Watcher::spawn(path.clone(), Filter::default());
                    if let Err(e) = r {
                        app.flash_error(&format!("board: {e}"));
                    }
                }
                Action::Open { plan, attempt, cols, rows } => match &client {
                    Some(c) => {
                        tokio::spawn(open_pane(c.clone(), path.clone(), plan, attempt, (cols, rows), tx.clone()));
                    }
                    None => {
                        let pane = plan.pane().to_string();
                        app.on_opened(&pane, attempt, Err("not connected to herdr-core".into()));
                    }
                },
            }
        }

        // Tell the programs which pane has the keyboard.
        let focused = app.focused_pane();
        if focused != last_focus {
            for (id, gained) in [(&last_focus, false), (&focused, true)] {
                if let Some(h) = id.as_ref().and_then(|id| handles.get(id)) {
                    let _ = h.cmd.send(PaneCmd::Focus(gained));
                }
            }
            last_focus = focused;
        }

        term.draw(|f| ws_ui::draw(f, &app))?;

        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(k))) if k.kind != KeyEventKind::Release => {
                    if let Some(out) = app.on_key(k) {
                        let bytes = match out {
                            Out::Key(k) => keys::encode(k, app.focused_app_cursor()),
                            Out::Bytes(b) => Some(b),
                        };
                        if let Some(b) = bytes {
                            send_input(&handles, &app, b);
                        }
                    }
                }
                Some(Ok(TermEvent::Paste(text))) => {
                    if let Some(text) = app.on_paste(&text) {
                        let bytes = if app.focused_bracketed_paste() {
                            format!("\x1b[200~{text}\x1b[201~")
                        } else {
                            text
                        };
                        send_input(&handles, &app, bytes.into_bytes());
                    }
                }
                // The next pass round the loop recomputes every pane's size.
                Some(Ok(TermEvent::Resize(_, _))) => {}
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
                    notify::alert(&a.what, &a.title, &a.body);
                }
            }
            Some(msg) = rx.recv() => match msg {
                Msg::Connected(Ok((c, _name))) => {
                    connecting = false;
                    let (c2, tx) = (c.clone(), tx.clone());
                    tokio::spawn(async move {
                        let _ = tx.send(Msg::Doc(workspace::load(&c2).await));
                    });
                    client = Some(c);
                    app.on_connected();
                }
                Msg::Connected(Err(e)) => {
                    connecting = false;
                    tracing::debug!("request connection failed: {e}");
                    next_connect = Instant::now() + RECONNECT_DELAY;
                }
                Msg::Doc(Ok(doc)) | Msg::Updated(Ok(doc)) => app.on_doc(doc),
                Msg::Doc(Err(e)) => {
                    tracing::debug!("load doc: {e}");
                    app.flash_error(&format!("can't read workspaces: {e}"));
                }
                Msg::Updated(Err(e)) => app.flash_error(&e),
                Msg::Started { pane, result } => app.on_started(&pane, result),
                Msg::Opened { pane: id, attempt, addr, cols, rows, result } => match result {
                    Ok(stream) if app.on_opened(&id, attempt, Ok((addr, cols, rows))) => {
                        let (cmd, cmds) = mpsc::unbounded_channel();
                        // The focus event of a pane that was attached just now.
                        let _ = cmd.send(PaneCmd::Focus(app.focused_pane().as_deref() == Some(id.as_str())));
                        tokio::spawn(pane_task(id.clone(), attempt, *stream, cmds, tx.clone()));
                        handles.insert(id, Handle { attempt, cmd });
                    }
                    // The person moved on meanwhile.
                    Ok(stream) => {
                        tokio::spawn(stream.close());
                    }
                    Err(e) => {
                        app.on_opened(&id, attempt, Err(e));
                    }
                },
                Msg::Stream { pane: id, attempt, msg } => {
                    if handles.get(&id).is_some_and(|h| h.attempt == attempt) {
                        if matches!(msg, StreamMsg::Closed(_)) {
                            handles.remove(&id);
                        }
                        app.on_stream(&id, msg);
                    }
                }
            },
            _ = slow.tick() => app.on_tick(), // also lets a message in the bottom bar expire
        }
        if app.quit {
            break;
        }
    }
    if remember {
        let mut state = config::load_ui_state();
        state.workspace = app.open.clone();
        state.sidebar_width = Some(app.sidebar_w);
        state.sidebar_hidden = app.sidebar_hidden;
        config::save_ui_state(&state);
    }
    // Detach every stream cleanly; the programs keep running.
    for (_, h) in handles.drain() {
        let _ = h.cmd.send(PaneCmd::Close);
    }
    tokio::time::sleep(Duration::from_millis(350)).await;
    Ok(())
}

/// Type into the focused pane's program.
fn send_input(handles: &HashMap<String, Handle>, app: &WsApp, bytes: Vec<u8>) {
    if let Some(h) = app.focused_pane().and_then(|id| handles.get(&id)) {
        let _ = h.cmd.send(PaneCmd::Input(bytes));
    }
}

/// End a pane's program and remove its terminal from hc. A just-killed
/// terminal may take a moment before it can be removed.
async fn kill_and_remove(c: &Client, addr: &str) {
    let _ = c.kill(addr).await;
    for _ in 0..10 {
        match c.rm(addr).await {
            Ok(_) => return,
            Err(e) if e.is_kind(ErrorKind::NotFound) => return,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Start a terminal for a pane that is not on screen and record it. The
/// document update goes to the loop before `Started`, so the pane already
/// has its address when the start counts as done.
async fn start_pane(c: Client, pane: String, params: hc_client::RunParams, tx: mpsc::UnboundedSender<Msg>) {
    let result = match c.run(&params).await {
        Ok(t) => {
            let op = WsOp::SetTerminal { pane: pane.clone(), terminal: Some(t.address.clone()) };
            match workspace::update(&c, &[op]).await {
                Ok(doc) => {
                    let _ = tx.send(Msg::Doc(Ok(doc)));
                }
                Err(e) => tracing::warn!("could not record terminal {}: {e}", t.address),
            }
            Ok(t.address)
        }
        Err(e) => Err(e.to_string()),
    };
    let _ = tx.send(Msg::Started { pane, result });
}

/// Start the pane's terminal if needed (recording it in the document), then
/// attach to it as a viewer of the pane's size.
async fn open_pane(c: Client, path: PathBuf, plan: Plan, attempt: u64, size: (u16, u16), tx: mpsc::UnboundedSender<Msg>) {
    let pane = plan.pane().to_string();
    let fail = |e: String| Msg::Opened { pane: pane.clone(), attempt, addr: String::new(), cols: 0, rows: 0, result: Err(e) };
    let addr = match plan {
        Plan::Attach { addr, .. } => addr,
        // Nothing to attach to; the app never asks for this.
        Plan::Ended { .. } => return,
        Plan::Start { pane, params } => match c.run(&params).await {
            Ok(t) => {
                let op = WsOp::SetTerminal { pane, terminal: Some(t.address.clone()) };
                match workspace::update(&c, &[op]).await {
                    Ok(doc) => {
                        let _ = tx.send(Msg::Doc(Ok(doc)));
                    }
                    Err(e) => tracing::warn!("could not record terminal {}: {e}", t.address),
                }
                t.address
            }
            Err(e) => {
                let _ = tx.send(fail(format!("can't start a terminal: {e}")));
                return;
            }
        },
    };
    let params = AttachParams::new(addr.clone(), size.0, size.1);
    let result = AttachStream::open_at(&path, &params).await.map(Box::new).map_err(|e| e.to_string());
    let _ = tx.send(Msg::Opened { pane: pane.clone(), attempt, addr, cols: size.0, rows: size.1, result });
}
