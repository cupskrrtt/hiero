//! Full-screen attach: Hiero steps aside and the agent gets the whole
//! terminal, like `hc attach`. Ctrl+\ (or Ctrl+]) comes back.

use crate::keys;
use anyhow::Result;
use crossterm::{execute, terminal};
use hc_client::{AttachParams, AttachStream, StreamMsg};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

pub enum Outcome {
    Detached,
    /// The program ended or the stream was lost; a short reason.
    Ended(String),
}

/// Run an attach session on the real terminal. The caller must have left its
/// own alternate screen and raw mode; this sets and restores both.
pub async fn fullscreen(socket: &Path, terminal_addr: &str) -> Result<Outcome> {
    let (cols, rows) = terminal::size()?;
    let mut stream = AttachStream::open_at(socket, &AttachParams::new(terminal_addr, cols, rows)).await?;

    let mut out = std::io::stdout();
    terminal::enable_raw_mode()?;
    execute!(out, terminal::EnterAlternateScreen)?;

    let (tx, mut input) = mpsc::unbounded_channel();
    let pump = StdinPump::spawn(tx);
    let mut winch = signal(SignalKind::window_change())?;
    let _ = stream.focus(true).await;

    let outcome = loop {
        tokio::select! {
            msg = stream.recv() => match msg {
                StreamMsg::Data(bytes) => {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                StreamMsg::Resize { .. } => {}
                StreamMsg::Closed(reason) => break Outcome::Ended(describe_close(reason.as_ref())),
            },
            Some(bytes) = input.recv() => {
                match keys::find_detach(&bytes) {
                    Some(i) => {
                        if i > 0 {
                            let _ = stream.input(&bytes[..i]).await;
                        }
                        break Outcome::Detached;
                    }
                    None => {
                        if stream.input(&bytes).await.is_err() {
                            break Outcome::Ended("connection lost".into());
                        }
                    }
                }
            }
            _ = winch.recv() => {
                if let Ok((c, r)) = terminal::size() {
                    let _ = stream.resize(c, r).await;
                }
            }
        }
    };

    drop(pump);
    if matches!(outcome, Outcome::Detached) {
        stream.close().await;
    }
    reset_modes(&mut out)?;
    execute!(out, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    Ok(outcome)
}

pub fn describe_close(reason: Option<&serde_json::Value>) -> String {
    let Some(r) = reason else {
        return "stream closed".into();
    };
    let why = r.get("reason").and_then(|v| v.as_str()).unwrap_or("closed");
    match r.get("exit_code").and_then(|v| v.as_i64()) {
        Some(code) => format!("{why} (code {code})"),
        None => why.replace('_', " "),
    }
}

/// Undo terminal modes the agent may have turned on through the passthrough.
fn reset_modes(out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(
        concat!(
            "\x1b[<u",                                            // pop kitty keyboard flags
            "\x1b[?1l\x1b>",                                     // normal cursor keys + keypad
            "\x1b[?2004l",                                       // bracketed paste off
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l",      // mouse off
            "\x1b[?1004l",                                       // focus events off
            "\x1b[?25h",                                         // show cursor
            "\x1b[0 q",                                          // default cursor shape
            "\x1b[0m",                                           // reset colors
        )
        .as_bytes(),
    )?;
    out.flush()
}

/// Reads raw bytes from stdin on a thread that can be stopped, so it never
/// steals a keystroke after the session ends.
struct StdinPump {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StdinPump {
    fn spawn(tx: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !flag.load(Ordering::Relaxed) {
                let mut pfd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd, short timeout.
                let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
                if ready <= 0 || pfd.revents & libc::POLLIN == 0 {
                    continue;
                }
                // SAFETY: buf is valid for buf.len() bytes.
                let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 || tx.send(buf[..n as usize].to_vec()).is_err() {
                    break;
                }
            }
        });
        StdinPump {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for StdinPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
