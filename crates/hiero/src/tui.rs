//! The board's event loop: keys, core events, screen reads and timers.

use crate::app::App;
use crate::{notify, ui};
use anyhow::Result;
use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind};
use futures::StreamExt;
use hc_client::{Client, Filter, WatchMsg, Watcher, endpoint};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

/// History lines to fetch above the visible screen for the preview.
const PREVIEW_HISTORY: usize = 200;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

pub async fn run() -> Result<()> {
    let path = endpoint::socket_path();
    let mut watch = Watcher::spawn(path.clone(), Filter::default());
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut watch, path).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    watch: &mut mpsc::Receiver<WatchMsg>,
    path: std::path::PathBuf,
) -> Result<()> {
    let mut app = App::new();
    let mut keys = EventStream::new();

    // A separate request connection for reads (the watcher's is events-only).
    let mut client: Option<Client> = None;
    let mut connecting = false;
    let mut next_connect = Instant::now();
    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<Result<(Client, String), String>>();
    let (prev_tx, mut prev_rx) = mpsc::unbounded_channel::<(String, Result<String, String>, bool)>();

    let mut anim = interval(Duration::from_millis(120));
    let mut slow = interval(Duration::from_secs(1));

    loop {
        if client.is_none() && !connecting && app.disconnected.is_none() && Instant::now() >= next_connect {
            connecting = true;
            let (tx, path) = (conn_tx.clone(), path.clone());
            tokio::spawn(async move {
                let r = async {
                    let c = Client::connect_to(&path).await?;
                    let name = c.hello().await?.core.name;
                    Ok::<_, hc_client::Error>((c, name))
                }
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(r);
            });
        }
        if let Some(c) = &client
            && let Some(addr) = app.take_preview_request()
        {
            let (c, tx) = (c.clone(), prev_tx.clone());
            tokio::spawn(async move {
                let (r, lost) = match c.read(&addr, PREVIEW_HISTORY).await {
                    Ok(r) => (Ok(r.text), false),
                    Err(e) => {
                        let lost = matches!(e, hc_client::Error::Disconnected | hc_client::Error::Io(_));
                        (Err(e.to_string()), lost)
                    }
                };
                let _ = tx.send((addr, r, lost));
            });
        }

        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(k))) if k.kind == KeyEventKind::Press => app.on_key(k),
                Some(Ok(_)) => {} // resize and friends: just redraw
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
            Some(r) = conn_rx.recv() => {
                connecting = false;
                match r {
                    Ok((c, name)) => {
                        client = Some(c);
                        app.core_name = name;
                        app.preview.dirty = true;
                    }
                    Err(e) => {
                        tracing::debug!("request connection failed: {e}");
                        next_connect = Instant::now() + RECONNECT_DELAY;
                    }
                }
            }
            Some((addr, r, lost)) = prev_rx.recv() => {
                if lost {
                    client = None; // likely dead; reconnect on the next pass
                    next_connect = Instant::now() + RECONNECT_DELAY;
                }
                app.on_preview(addr, r);
            }
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
    Ok(())
}
