//! Plain-mode JSON-RPC connection: one JSON message per line.

use crate::endpoint;
use crate::error::{Error, Result, WireError};
use crate::events::{Notification, Subscription};
use crate::types::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value>>>>>>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// A connection to a herdr-core. Cheap to clone; calls may run concurrently.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    writer: tokio::sync::Mutex<Writer>,
    /// `None` once the connection is gone.
    pending: Pending,
    next_id: AtomicU64,
}

impl Client {
    /// Connect to the default endpoint (see [`endpoint::socket_path`]).
    pub async fn connect() -> Result<Self> {
        Self::connect_to(&endpoint::socket_path()).await
    }

    pub async fn connect_to(path: &Path) -> Result<Self> {
        let (client, _) = Self::open(path, false).await?;
        Ok(client)
    }

    async fn open(
        path: &Path,
        notifications: bool,
    ) -> Result<(Self, Option<mpsc::UnboundedReceiver<Notification>>)> {
        let (r, w) = crate::transport::connect(path)
            .await
            .map_err(|source| Error::Connect {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Self::from_parts(r, w, notifications))
    }

    /// Build a client over any byte stream (an `ssh box hc connect` pipe, a test duplex).
    pub fn from_parts<R, W>(
        reader: R,
        writer: W,
        notifications: bool,
    ) -> (Self, Option<mpsc::UnboundedReceiver<Notification>>)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (ntx, nrx) = if notifications {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        tokio::spawn(read_loop(BufReader::new(reader), pending.clone(), ntx));
        let client = Client {
            inner: Arc::new(Inner {
                writer: tokio::sync::Mutex::new(Box::new(writer)),
                pending,
                next_id: AtomicU64::new(1),
            }),
        };
        (client, nrx)
    }

    /// Call any method and decode its result.
    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let value = self.call_raw(method, params).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Call any method and return its raw result.
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.inner.pending.lock().unwrap();
            let map = guard.as_mut().ok_or(Error::Disconnected)?;
            map.insert(id, tx);
        }
        let mut line =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        line.push(b'\n');
        tracing::trace!(method, id, "call");
        let write = async {
            let mut w = self.inner.writer.lock().await;
            w.write_all(&line).await?;
            w.flush().await
        };
        if let Err(e) = write.await {
            if let Some(map) = self.inner.pending.lock().unwrap().as_mut() {
                map.remove(&id);
            }
            return Err(e.into());
        }
        rx.await.map_err(|_| Error::Disconnected)?
    }

    // ---- server -------------------------------------------------------------

    pub async fn hello(&self) -> Result<HelloResult> {
        self.call("hello", json!({})).await
    }

    pub async fn status(&self) -> Result<StatusResult> {
        self.call("server.status", json!({})).await
    }

    // ---- terminals ----------------------------------------------------------

    /// Live terminals; `all` includes finished ones.
    pub async fn list(&self, all: bool, labels: &Labels) -> Result<ListResult> {
        self.call("terminal.list", json!({"all": all, "labels": labels}))
            .await
    }

    pub async fn get(&self, terminal: &str) -> Result<Terminal> {
        self.call("terminal.get", json!({"terminal": terminal})).await
    }

    pub async fn run(&self, params: &RunParams) -> Result<Terminal> {
        self.call("terminal.run", serde_json::to_value(params)?)
            .await
    }

    /// The screen plus `lines` of history.
    pub async fn read(&self, terminal: &str, lines: usize) -> Result<ReadResult> {
        self.call("terminal.read", json!({"terminal": terminal, "lines": lines}))
            .await
    }

    /// Paste text and press Enter (once nobody has typed for a moment).
    pub async fn send(&self, terminal: &str, text: &str) -> Result<SendResult> {
        self.call("terminal.send", json!({"terminal": terminal, "text": text}))
            .await
    }

    /// Like [`send`](Self::send); `force` types even while the agent is blocked
    /// (it may be answering its own dialog).
    pub async fn send_with(&self, terminal: &str, text: &str, force: bool) -> Result<SendResult> {
        self.call(
            "terminal.send",
            json!({"terminal": terminal, "text": text, "force": force}),
        )
        .await
    }

    /// Press named keys: `enter`, `esc`, `ctrl+c`, `up`, …
    pub async fn keys(&self, terminal: &str, keys: &[&str]) -> Result<KeysResult> {
        self.call("terminal.keys", json!({"terminal": terminal, "keys": keys}))
            .await
    }

    pub async fn interrupt(&self, terminal: &str) -> Result<InterruptResult> {
        self.call("terminal.interrupt", json!({"terminal": terminal}))
            .await
    }

    pub async fn kill(&self, terminal: &str) -> Result<Terminal> {
        self.call("terminal.kill", json!({"terminal": terminal}))
            .await
    }

    /// Remove a finished terminal.
    pub async fn rm(&self, terminal: &str) -> Result<RemoveResult> {
        self.call("terminal.rm", json!({"terminal": terminal}))
            .await
    }

    /// Set labels; an empty value removes that key.
    pub async fn label(&self, terminal: &str, labels: &Labels) -> Result<Terminal> {
        self.call(
            "terminal.label",
            json!({"terminal": terminal, "labels": labels}),
        )
        .await
    }

    pub async fn keep(&self, terminal: &str, keep: bool) -> Result<Terminal> {
        self.call("terminal.keep", json!({"terminal": terminal, "keep": keep}))
            .await
    }

    pub async fn machines(&self) -> Result<MachineListResult> {
        self.call("machine.list", json!({})).await
    }

    pub async fn templates(&self) -> Result<TemplatesResult> {
        self.call("templates.list", json!({})).await
    }

    // ---- documents ----------------------------------------------------------

    /// Read a JSON document; `{null, 0}` when never written.
    pub async fn doc_get(&self, name: &str) -> Result<DocGetResult> {
        self.call("doc.get", json!({"name": name})).await
    }

    /// Write a document if `version` still matches; fails with `ErrorKind::Conflict` otherwise.
    pub async fn doc_set(&self, name: &str, value: &Value, version: u64) -> Result<DocSetResult> {
        self.call(
            "doc.set",
            json!({"name": name, "value": value, "version": version}),
        )
        .await
    }

    // ---- events -------------------------------------------------------------

    /// Open a dedicated connection and subscribe to events.
    ///
    /// In plain mode a subscribed connection carries only notifications, so this
    /// never reuses `self`'s connection.
    pub async fn subscribe(filter: &crate::events::Filter) -> Result<Subscription> {
        Self::subscribe_at(&endpoint::socket_path(), filter).await
    }

    pub async fn subscribe_at(path: &Path, filter: &crate::events::Filter) -> Result<Subscription> {
        let (client, rx) = Self::open(path, true).await?;
        Subscription::start(client, rx.expect("notifications enabled"), filter).await
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: BufReader<R>,
    pending: Pending,
    notifications: Option<mpsc::UnboundedSender<Notification>>,
) {
    let mut lines = reader.lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("hc read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("hc sent bad json: {e}");
                continue;
            }
        };

        // A response has an id; a notification has a method and no id.
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let result = if let Some(err) = msg.get("error") {
                match serde_json::from_value::<WireError>(err.clone()) {
                    Ok(w) => Err(Error::Rpc(w.into())),
                    Err(e) => Err(Error::Json(e)),
                }
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            let tx = pending.lock().unwrap().as_mut().and_then(|m| m.remove(&id));
            match tx {
                Some(tx) => {
                    let _ = tx.send(result);
                }
                None => tracing::debug!(id, "reply for unknown id"),
            }
        } else if let Some(tx) = &notifications {
            match Notification::decode(msg) {
                Ok(n) => {
                    if tx.send(n).is_err() {
                        break; // subscriber dropped
                    }
                }
                Err(e) => tracing::warn!("bad notification: {e}"),
            }
        }
    }
    // Fail everything still waiting, and refuse new calls.
    if let Some(map) = pending.lock().unwrap().take() {
        for (_, tx) in map {
            let _ = tx.send(Err(Error::Disconnected));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A fake core on the other end of a duplex pipe.
    fn fake_core<F>(respond: F) -> Client
    where
        F: Fn(&Value) -> Value + Send + 'static,
    {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let (r, w) = tokio::io::split(ours);
        let (client, _) = Client::from_parts(r, w, false);
        tokio::spawn(async move {
            let (tr, mut tw) = tokio::io::split(theirs);
            let mut lines = BufReader::new(tr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&l).unwrap();
                let mut out = serde_json::to_vec(&respond(&req)).unwrap();
                out.push(b'\n');
                tw.write_all(&out).await.unwrap();
            }
        });
        client
    }

    #[tokio::test]
    async fn matches_replies_and_decodes_errors() {
        let client = fake_core(|req| {
            let id = req["id"].clone();
            match req["method"].as_str().unwrap() {
                "doc.get" => json!({"jsonrpc":"2.0","id":id,"result":{"value":null,"version":0}}),
                _ => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"no terminal t9","data":{"kind":"not_found"}}}),
            }
        });

        let (a, b) = tokio::join!(client.doc_get("hiero"), client.get("t9"));
        assert_eq!(a.unwrap().version, 0);
        assert!(b.unwrap_err().is_kind(ErrorKind::NotFound));
    }

    #[tokio::test]
    async fn pending_calls_fail_when_core_goes_away() {
        let (ours, theirs) = tokio::io::duplex(1024);
        let (r, w) = tokio::io::split(ours);
        let (client, _) = Client::from_parts(r, w, false);
        let call = tokio::spawn({
            let c = client.clone();
            async move { c.status().await }
        });
        tokio::task::yield_now().await;
        drop(theirs);
        assert!(matches!(call.await.unwrap(), Err(Error::Disconnected)));
        assert!(matches!(client.status().await, Err(Error::Disconnected)));
    }
}
