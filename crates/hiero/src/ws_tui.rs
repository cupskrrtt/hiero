//! The workspace screen's event loop: keys, core events, the attached pane.
//! Same shape as the board's loop (`tui`), without the board's state.

use crate::workspace::{self, WsDoc, WsOp};
use crate::ws_app::{Action, Out, Plan, WsApp};
use crate::{attach, config, keys, notify, ws_ui};
use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyEventKind};
use crossterm::execute;
use futures::StreamExt;
use hc_client::{AttachParams, AttachStream, Client, Filter, StreamMsg, WatchMsg, Watcher, endpoint};
use ratatui::layout::Rect;
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
    Opened { pane: String, addr: String, cols: u16, rows: u16, result: Result<Box<AttachStream>, String> },
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

fn pane_size(term: &ratatui::DefaultTerminal, banner: bool) -> Result<Rect> {
    let size = term.size()?;
    Ok(ws_ui::pane_inner(Rect::new(0, 0, size.width, size.height), banner))
}

async fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    watch: &mut mpsc::Receiver<WatchMsg>,
    path: PathBuf,
    config_error: Option<String>,
) -> Result<()> {
    let remember = config::get().board.remember;
    let restore = if remember { config::load_ui_state().workspace } else { None };
    let mut app = WsApp::new(config::get().prefix(), restore);
    if let Some(e) = config_error {
        tracing::warn!("{e}");
        app.flash_error(&format!("config ignored: {e}"));
    }
    let mut keys = EventStream::new();

    let mut client: Option<Client> = None;
    let mut connecting = false;
    let mut next_connect = Instant::now();
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    // The attach stream of the pane on screen.
    let mut pane: Option<AttachStream> = None;
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

        for action in std::mem::take(&mut app.actions) {
            match action {
                Action::Detach => {
                    if let Some(p) = pane.take() {
                        tokio::spawn(p.close());
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
                Action::Open(plan) => match &client {
                    Some(c) => {
                        let inner = pane_size(term, app.disconnected.is_some())?;
                        tokio::spawn(open_pane(c.clone(), path.clone(), plan, inner, tx.clone()));
                    }
                    None => app.pane_failed("not connected to herdr-core".into()),
                },
            }
        }

        term.draw(|f| ws_ui::draw(f, &app))?;

        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(k))) if k.kind != KeyEventKind::Release => {
                    if let Some(out) = app.on_key(k)
                        && let (Some(p), Some(view)) = (&pane, &app.pane)
                    {
                        let bytes = match out {
                            Out::Key(k) => keys::encode(k, view.parser.screen().application_cursor()),
                            Out::Bytes(b) => Some(b),
                        };
                        if let Some(b) = bytes {
                            let _ = p.input(&b).await;
                        }
                    }
                }
                Some(Ok(TermEvent::Paste(text))) => {
                    if let Some(text) = app.on_paste(&text)
                        && let (Some(p), Some(view)) = (&pane, &app.pane)
                    {
                        let bytes = if view.parser.screen().bracketed_paste() {
                            format!("\x1b[200~{text}\x1b[201~")
                        } else {
                            text
                        };
                        let _ = p.input(bytes.as_bytes()).await;
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
                    notify::alert(&a.what, &a.title, &a.body);
                }
            }
            msg = crate::tui::pane_recv(&mut pane) => {
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
                Msg::Connected(Ok((c, _name))) => {
                    connecting = false;
                    let (c2, tx) = (c.clone(), tx.clone());
                    tokio::spawn(async move {
                        let _ = tx.send(Msg::Doc(workspace::load(&c2).await));
                    });
                    client = Some(c);
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
                Msg::Opened { pane: id, addr, cols, rows, result } => match result {
                    Ok(stream) if app.wants(&id) => {
                        app.pane_attached(addr, cols, rows);
                        let _ = stream.focus(true).await;
                        pane = Some(*stream);
                    }
                    // The person moved to another workspace meanwhile.
                    Ok(stream) => {
                        tokio::spawn(stream.close());
                    }
                    Err(e) if app.wants(&id) => app.pane_failed(e),
                    Err(_) => {}
                },
            },
            _ = slow.tick() => {} // lets a message in the bottom bar expire
        }
        if app.quit {
            break;
        }
    }
    if remember {
        let mut state = config::load_ui_state();
        state.workspace = app.open.clone();
        config::save_ui_state(&state);
    }
    if let Some(p) = pane.take() {
        p.close().await;
    }
    Ok(())
}

/// Start the pane's terminal if needed (recording it in the document), then
/// attach to it as a viewer of the pane's size.
async fn open_pane(c: Client, path: PathBuf, plan: Plan, inner: Rect, tx: mpsc::UnboundedSender<Msg>) {
    let pane = plan.pane().to_string();
    let fail = |e: String| Msg::Opened { pane: pane.clone(), addr: String::new(), cols: 0, rows: 0, result: Err(e) };
    let addr = match plan {
        Plan::Attach { addr, .. } => addr,
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
    let params = AttachParams::new(addr.clone(), inner.width, inner.height);
    let result = AttachStream::open_at(&path, &params).await.map(Box::new).map_err(|e| e.to_string());
    let _ = tx.send(Msg::Opened { pane: pane.clone(), addr, cols: inner.width, rows: inner.height, result });
}
