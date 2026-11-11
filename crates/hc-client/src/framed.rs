//! Framed mode, used for attach streams.
//!
//! Every frame: `[channel u32 LE][kind u8][len u32 LE][payload]`. Channel 0
//! carries JSON-RPC; other channels carry a terminal's raw bytes.

use crate::endpoint;
use crate::error::{Error, Result, WireError};
use crate::transport::{self, Reader, Writer};
use crate::types::Terminal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

pub const DATA: u8 = 0;
pub const CREDIT: u8 = 1;
pub const CLOSE: u8 = 2;
pub const RESIZE: u8 = 3;
pub const FOCUS: u8 = 4;

const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// What arrives on an attach stream.
#[derive(Debug, Clone)]
pub enum StreamMsg {
    /// Program output (the first one is the current screen).
    Data(Vec<u8>),
    /// The terminal's grid changed (observers with resize notifications).
    Resize { cols: u16, rows: u16 },
    /// The stream ended; the core's reason, e.g. `{"reason":"exited","exit_code":0}`.
    Closed(Option<Value>),
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachParams {
    pub terminal: String,
    pub cols: u16,
    pub rows: u16,
    /// Watch only: never type or resize.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub observe: bool,
    /// Scrollback lines to paint first.
    #[serde(skip_serializing_if = "is_zero")]
    pub history: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl AttachParams {
    pub fn new(terminal: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            terminal: terminal.into(),
            cols,
            rows,
            observe: false,
            history: 0,
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

#[derive(Default)]
struct Routes {
    streams: HashMap<u32, mpsc::UnboundedSender<StreamMsg>>,
    /// Frames that arrived before their stream was claimed.
    early: HashMap<u32, Vec<StreamMsg>>,
    /// Channels whose CLOSE was delivered: later frames for them are dropped,
    /// not kept in `early` for the life of the connection.
    ended: HashSet<u32>,
    closed: bool,
}

/// One framed connection. Each attach gets its own, so dropping the stream
/// closes everything.
struct Conn {
    writer: tokio::sync::Mutex<Writer>,
    pending: Pending,
    routes: Arc<Mutex<Routes>>,
    next_id: AtomicU64,
    /// The task reading the connection; ended on drop (a Windows named pipe
    /// stays open while its read half is blocked in a read).
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Conn {
    async fn open(path: &Path) -> Result<Arc<Conn>> {
        let (r, mut w) = transport::connect(path)
            .await
            .map_err(|source| Error::Connect {
                path: path.display().to_string(),
                source,
            })?;
        let mut r = BufReader::new(r);

        // Plain-mode hello asking to switch; its reply is the last plain line.
        let hello = json!({"jsonrpc":"2.0","id":0,"method":"hello","params":{"framed":true}});
        let mut line = serde_json::to_vec(&hello)?;
        line.push(b'\n');
        w.write_all(&line).await?;
        let mut reply = String::new();
        if r.read_line(&mut reply).await? == 0 {
            return Err(Error::Disconnected);
        }
        let reply: Value = serde_json::from_str(&reply)?;
        if let Some(err) = reply.get("error") {
            let w: WireError = serde_json::from_value(err.clone())?;
            return Err(Error::Rpc(w.into()));
        }
        if reply.pointer("/result/framed") != Some(&Value::Bool(true)) {
            return Err(Error::Protocol("core did not switch to framed mode".into()));
        }

        let (pending, routes): (Pending, Arc<Mutex<Routes>>) = Default::default();
        let reader = tokio::spawn(read_loop(r, pending.clone(), routes.clone()));
        Ok(Arc::new(Conn {
            writer: tokio::sync::Mutex::new(w),
            pending,
            routes,
            next_id: AtomicU64::new(1),
            reader,
        }))
    }

    async fn frame(&self, channel: u32, kind: u8, payload: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(9 + payload.len());
        buf.extend_from_slice(&channel.to_le_bytes());
        buf.push(kind);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);
        let mut w = self.writer.lock().await;
        w.write_all(&buf).await?;
        w.flush().await?;
        Ok(())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        if let Err(e) = self.frame(0, DATA, &msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        rx.await.map_err(|_| Error::Disconnected)?
    }

    fn claim(&self, channel: u32) -> mpsc::UnboundedReceiver<StreamMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut routes = self.routes.lock().unwrap();
        let mut ended = false;
        for msg in routes.early.remove(&channel).unwrap_or_default() {
            ended |= matches!(msg, StreamMsg::Closed(_));
            let _ = tx.send(msg);
        }
        if ended {
            // Its CLOSE came before we claimed it; nothing more will arrive.
        } else if routes.closed {
            let _ = tx.send(StreamMsg::Closed(None));
        } else {
            routes.streams.insert(channel, tx);
        }
        rx
    }
}

async fn read_loop(mut r: BufReader<Reader>, pending: Pending, routes: Arc<Mutex<Routes>>) {
    let mut header = [0u8; 9];
    loop {
        if r.read_exact(&mut header).await.is_err() {
            break;
        }
        let channel = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let kind = header[4];
        let len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        if len > MAX_PAYLOAD {
            tracing::warn!(len, "oversized frame; dropping connection");
            break;
        }
        let mut payload = vec![0u8; len];
        if r.read_exact(&mut payload).await.is_err() {
            break;
        }

        if channel == 0 {
            if kind != DATA {
                continue;
            }
            let Ok(mut msg) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                // Take the parts out instead of copying them.
                let result = match msg.as_object_mut().and_then(|m| m.remove("error")) {
                    Some(err) => match serde_json::from_value::<WireError>(err) {
                        Ok(w) => Err(Error::Rpc(w.into())),
                        Err(e) => Err(Error::Json(e)),
                    },
                    None => Ok(msg.as_object_mut().and_then(|m| m.remove("result")).unwrap_or(Value::Null)),
                };
                if let Some(tx) = pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
            continue;
        }

        let msg = match kind {
            DATA => StreamMsg::Data(payload),
            RESIZE if payload.len() >= 4 => StreamMsg::Resize {
                cols: u16::from_le_bytes([payload[0], payload[1]]),
                rows: u16::from_le_bytes([payload[2], payload[3]]),
            },
            CLOSE => StreamMsg::Closed(serde_json::from_slice(&payload).ok()),
            _ => continue,
        };
        let closing = matches!(msg, StreamMsg::Closed(_));
        let mut routes = routes.lock().unwrap();
        match routes.streams.get(&channel) {
            Some(tx) => {
                let _ = tx.send(msg);
            }
            None if routes.ended.contains(&channel) => {}
            None => routes.early.entry(channel).or_default().push(msg),
        }
        if closing {
            routes.streams.remove(&channel);
            routes.ended.insert(channel);
        }
    }

    let mut routes = routes.lock().unwrap();
    routes.closed = true;
    for (_, tx) in routes.streams.drain() {
        let _ = tx.send(StreamMsg::Closed(None));
    }
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(Error::Disconnected));
    }
}

#[derive(Deserialize)]
struct AttachReply {
    channel: u32,
    terminal: Terminal,
}

/// A live stream to one terminal, on its own connection.
pub struct AttachStream {
    pub channel: u32,
    /// The terminal as it was when attached.
    pub terminal: Terminal,
    conn: Arc<Conn>,
    rx: mpsc::UnboundedReceiver<StreamMsg>,
    closed: bool,
}

impl AttachStream {
    pub async fn open(params: &AttachParams) -> Result<Self> {
        Self::open_at(&endpoint::socket_path(), params).await
    }

    pub async fn open_at(path: &Path, params: &AttachParams) -> Result<Self> {
        let conn = Conn::open(path).await?;
        let reply: AttachReply = serde_json::from_value(
            conn.call("terminal.attach", serde_json::to_value(params)?).await?,
        )?;
        let rx = conn.claim(reply.channel);
        Ok(AttachStream {
            channel: reply.channel,
            terminal: reply.terminal,
            conn,
            rx,
            closed: false,
        })
    }

    /// The next message, without crediting it: output stays counted against
    /// the core's window until [`credit`](Self::credit) says it was consumed.
    /// Cancel-safe (nothing is lost if the future is dropped while waiting).
    pub async fn next(&mut self) -> StreamMsg {
        if self.closed {
            return StreamMsg::Closed(None);
        }
        let msg = self.rx.recv().await.unwrap_or(StreamMsg::Closed(None));
        if matches!(msg, StreamMsg::Closed(_)) {
            self.closed = true;
        }
        msg
    }

    /// Give the core back `bytes` of the output window (after consuming a `Data` chunk).
    pub async fn credit(&self, bytes: usize) {
        let n = (bytes as u32).to_le_bytes();
        let _ = self.conn.frame(self.channel, CREDIT, &n).await;
    }

    /// The next message, credited as soon as it's handed over. NOT cancel-safe
    /// (a drop during the credit write loses the message): don't `select!` on
    /// it; use [`next`](Self::next) and [`credit`](Self::credit) there.
    pub async fn recv(&mut self) -> StreamMsg {
        let msg = self.next().await;
        if let StreamMsg::Data(bytes) = &msg {
            self.credit(bytes.len()).await;
        }
        msg
    }

    /// Send key bytes, exactly as a terminal would.
    pub async fn input(&self, bytes: &[u8]) -> Result<()> {
        self.conn.frame(self.channel, DATA, bytes).await
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let mut p = [0u8; 4];
        p[..2].copy_from_slice(&cols.to_le_bytes());
        p[2..].copy_from_slice(&rows.to_le_bytes());
        self.conn.frame(self.channel, RESIZE, &p).await
    }

    pub async fn focus(&self, gained: bool) -> Result<()> {
        self.conn.frame(self.channel, FOCUS, &[gained as u8]).await
    }

    /// Detach. The program keeps running.
    pub async fn close(mut self) {
        if !self.closed {
            let _ = self.conn.frame(self.channel, CLOSE, &[]).await;
            // Wait briefly for the core's empty CLOSE so it knows we left cleanly.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(300), async {
                loop {
                    if let StreamMsg::Closed(_) = self.rx.recv().await.unwrap_or(StreamMsg::Closed(None)) {
                        break;
                    }
                }
            })
            .await;
            self.closed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = channel.to_le_bytes().to_vec();
        b.push(kind);
        b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        b.extend_from_slice(payload);
        b
    }

    #[tokio::test]
    async fn frames_after_a_close_are_not_kept() {
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let routes: Arc<Mutex<Routes>> = Arc::default();
        let task = tokio::spawn(read_loop(BufReader::new(Box::new(ours) as Reader), Pending::default(), routes.clone()));
        // Output and the close arrive before anyone claims channel 5; more output after the close.
        for f in [frame(5, DATA, b"a"), frame(5, CLOSE, b"{}"), frame(5, DATA, b"late"), frame(5, DATA, b"later")] {
            theirs.write_all(&f).await.unwrap();
        }
        drop(theirs);
        task.await.unwrap();
        let routes = routes.lock().unwrap();
        let early = &routes.early[&5];
        assert_eq!(early.len(), 2, "only what came before the close is kept: {early:?}");
        assert!(matches!(early[1], StreamMsg::Closed(_)));
    }
}
