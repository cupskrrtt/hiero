//! The workspace screen's event loop: keys, core events, the attached panes.

use crate::workspace::{self, WsDoc, WsOp};
use crate::ws_app::{Action, Out, Plan, WsApp};
use crate::{config, keys, notify, ws_ui};
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event as TermEvent, EventStream,
};
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
/// The longest the screen goes without a redraw while panes stream output.
const FRAME: Duration = Duration::from_millis(16);

enum Msg {
    Connected(Result<(Client, String), String>),
    Doc(Result<WsDoc, String>),
    /// A change we made; an error is shown, the doc is taken as the new truth.
    Updated(Result<WsDoc, String>),
    /// A background start finished (its terminal is recorded in the doc first).
    Started { pane: String, result: Result<String, String> },
    Opened { pane: String, attempt: u64, addr: String, cols: u16, rows: u16, result: Result<Box<AttachStream>, String> },
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

/// Output or the end of a pane's stream, tagged with the attempt it belongs to.
struct StreamOut {
    pane: String,
    attempt: u64,
    msg: StreamMsg,
}

/// Chunks of pane output the loop has not consumed yet, over all panes. A
/// chunk is only credited back to the core once it is in here, so a program
/// that floods faster than we parse and draw is held back by the core's
/// flow control instead of piling up in memory. (Each pane's task may hold
/// one more chunk while it waits for room.)
const STREAM_QUEUE: usize = 64;

/// One task per attached pane owns its stream (`next` needs `&mut`) and
/// waits on both the stream and the loop's commands. Output goes to the loop
/// through the bounded `out` channel tagged with the pane and attempt. It
/// ends when the stream closes or the loop says so.
///
/// Every branch future is cancel-safe (`reserve`, `next`, `recv`); every
/// socket write happens in a branch body and runs to completion. A task
/// asks for room in the queue only while it holds a chunk: panes that are
/// attached but quiet (hidden tabs) must not use the queue up.
async fn pane_task(
    pane: String,
    attempt: u64,
    mut stream: AttachStream,
    mut cmds: mpsc::UnboundedReceiver<PaneCmd>,
    out: mpsc::Sender<StreamOut>,
) {
    // A chunk taken off the stream that waits for room in the queue.
    let mut held: Option<StreamMsg> = None;
    loop {
        tokio::select! {
            msg = stream.next(), if held.is_none() => held = Some(msg),
            room = out.reserve(), if held.is_some() => {
                let (Ok(room), Some(msg)) = (room, held.take()) else { return };
                let closed = matches!(msg, StreamMsg::Closed(_));
                let credit = if let StreamMsg::Data(b) = &msg { b.len() } else { 0 };
                room.send(StreamOut { pane: pane.clone(), attempt, msg });
                if credit > 0 {
                    stream.credit(credit).await;
                }
                if closed {
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
    // ratatui's own panic hook restores raw mode and the screen but not the
    // mouse; without this a crash leaves the shell printing mouse escapes.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        previous(info);
    }));
    // Some Windows consoles refuse bracketed paste (pastes then arrive as keys); that's fine.
    if let Err(e) = execute!(std::io::stdout(), EnableBracketedPaste) {
        tracing::debug!("bracketed paste unavailable: {e}");
    }
    if config::get().workspace.mouse {
        execute!(std::io::stdout(), EnableMouseCapture)?;
    }
    let result = event_loop(&mut term, &mut watch, path, config_error).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    watch: &mut mpsc::Receiver<WatchMsg>,
    path: PathBuf,
    config_error: Option<String>,
) -> Result<()> {
    let saved = config::load_ui_state();
    let mut app = WsApp::new(config::get().prefix(), saved.workspace);
    app.set_sidebar(saved.sidebar_width, saved.sidebar_hidden);
    app.auto_restore = config::get().workspace.restore;
    app.scrollback = config::get().workspace.scrollback;
    app.self_addr = std::env::var("HC_TERMINAL_ID").ok().filter(|s| !s.is_empty());
    if let Some(e) = config_error {
        tracing::warn!("{e}");
        app.flash_error(&format!("config ignored: {e}"));
    } else if let Some(w) = config::get().ignored_warning() {
        tracing::warn!("{w}");
        app.flash_info(&w);
    }
    let mut keys = EventStream::new();

    let mut client: Option<Client> = None;
    let mut connecting = false;
    let mut next_connect = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let (out_tx, mut out_rx) = mpsc::channel::<StreamOut>(STREAM_QUEUE);
    // The task of each attached pane, by pane id.
    let mut handles: HashMap<String, Handle> = HashMap::new();
    // The pane that last got the focus event.
    let mut last_focus: Option<String> = None;
    let mut slow = interval(Duration::from_secs(1));
    let mut last_draw = Instant::now() - FRAME;
    // The screen size, asked of the terminal at start and on a resize event only.
    let mut size = term.size()?;
    // Something visible may have changed since the last draw.
    let mut dirty = true;

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

        // Cheap unless the screen, the banner or the sidebar changed; a layout
        // change inside the app syncs the panes itself.
        app.set_screen(Rect::new(0, 0, size.width, size.height));

        let actions = std::mem::take(&mut app.actions);
        dirty |= !actions.is_empty();
        for action in actions {
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
                // After any resize on the same channel, so the terminal has the size first.
                Action::SizeReport { pane, cols, rows } => {
                    if let Some(h) = handles.get(&pane) {
                        let _ = h.cmd.send(PaneCmd::Input(format!("\x1b[48;{rows};{cols};0;0t").into_bytes()));
                    }
                }
                Action::Clipboard(text) => crate::copy::to_clipboard(&text),
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

        // Draw once the queue is drained, or at least every frame while a pane
        // streams fast. Drawing after every chunk can't keep up when output
        // floods in (slow consoles, ConPTY on Windows): the screen then lags
        // seconds behind and shows old frames mixed with new ones.
        if dirty && ((rx.is_empty() && out_rx.is_empty()) || last_draw.elapsed() >= FRAME) {
            term.draw(|f| ws_ui::draw(f, &app))?;
            last_draw = Instant::now();
            dirty = false;
        }

        let resync = app.next_resync().map(Instant::from_std);
        tokio::select! {
            // A resized pane has held still: take its screen again.
            _ = tokio::time::sleep_until(resync.unwrap_or_else(Instant::now)), if resync.is_some() => {
                app.resync_due(std::time::Instant::now());
            }
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(k))) if keys::is_input(k.kind) => {
                    dirty = true;
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
                    dirty = true;
                    if let Some(text) = app.on_paste(&text) {
                        let bytes = if app.focused_bracketed_paste() {
                            format!("\x1b[200~{text}\x1b[201~")
                        } else {
                            text
                        };
                        send_input(&handles, &app, bytes.into_bytes());
                    }
                }
                Some(Ok(TermEvent::Mouse(m))) => {
                    // A plain pointer move changes nothing we draw.
                    dirty |= !matches!(m.kind, crossterm::event::MouseEventKind::Moved);
                    if let Some((pane, bytes)) = app.on_mouse(m)
                        && let Some(h) = handles.get(&pane)
                    {
                        let _ = h.cmd.send(PaneCmd::Input(bytes));
                    }
                }
                // The next pass lays the panes out for the new size.
                Some(Ok(TermEvent::Resize(_, _))) => {
                    size = term.size()?;
                    dirty = true;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            msg = watch.recv() => {
                let Some(msg) = msg else { break };
                dirty = true;
                if matches!(msg, WatchMsg::Disconnected { .. }) {
                    client = None;
                }
                for a in app.on_watch(&msg) {
                    notify::alert(&a.what, &a.title, &a.body);
                }
            }
            Some(msg) = rx.recv() => { dirty = true; match msg {
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
                        tokio::spawn(pane_task(id.clone(), attempt, *stream, cmds, out_tx.clone()));
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
            } }
            Some(first) = out_rx.recv() => {
                // Output of a pane that is not on screen changes nothing we draw.
                dirty |= on_stream_out(&mut app, &mut handles, first);
                // Several chunks per pass: a full pass (actions, draw check) per chunk is wasted work under a flood.
                for _ in 0..STREAM_BATCH {
                    if last_draw.elapsed() >= FRAME {
                        break;
                    }
                    let Ok(next) = out_rx.try_recv() else { break };
                    dirty |= on_stream_out(&mut app, &mut handles, next);
                }
            }
            _ = slow.tick() => {
                app.on_tick(); // also lets a message in the bottom bar expire
                dirty = true;
            }
        }
        if app.quit {
            break;
        }
    }
    let mut state = config::load_ui_state();
    state.workspace = app.open.clone();
    state.sidebar_width = Some(app.sidebar_w);
    state.sidebar_hidden = app.sidebar_hidden;
    config::save_ui_state(&state);
    // Detach every stream cleanly; the programs keep running.
    for (_, h) in handles.drain() {
        let _ = h.cmd.send(PaneCmd::Close);
    }
    tokio::time::sleep(Duration::from_millis(350)).await;
    Ok(())
}

/// Most chunks taken off the stream queue in one pass of the loop.
const STREAM_BATCH: usize = 32;

/// A pane's output reaches its screen (if the pane's task is still the
/// current one). True when that changed something on screen.
fn on_stream_out(app: &mut WsApp, handles: &mut HashMap<String, Handle>, o: StreamOut) -> bool {
    let StreamOut { pane, attempt, msg } = o;
    if !handles.get(&pane).is_some_and(|h| h.attempt == attempt) {
        return false;
    }
    if matches!(msg, StreamMsg::Closed(_)) {
        handles.remove(&pane);
    }
    app.on_stream(&pane, msg)
}

/// Type into the focused pane's program.
fn send_input(handles: &HashMap<String, Handle>, app: &WsApp, bytes: Vec<u8>) {
    if let Some(h) = app.focused_pane().and_then(|id| handles.get(&id)) {
        let _ = h.cmd.send(PaneCmd::Input(bytes));
    }
}

/// End a pane's program and remove its terminal from hc. A just-killed
/// terminal may take a moment before it can be removed.
pub async fn kill_and_remove(c: &Client, addr: &str) {
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
    let mut params = AttachParams::new(addr.clone(), size.0, size.1);
    // The history lands above the screen, so a reattached pane scrolls back too.
    params.history = config::get().workspace.scrollback;
    let result = AttachStream::open_at(&path, &params).await.map(Box::new).map_err(|e| e.to_string());
    let _ = tx.send(Msg::Opened { pane: pane.clone(), attempt, addr, cols: size.0, rows: size.1, result });
}
