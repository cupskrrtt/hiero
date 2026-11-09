//! `events.subscribe`: a snapshot, then every change in order.

use crate::client::Client;
use crate::error::{Error, Result};
use crate::types::{Foreground, Labels, Status, Terminal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// Which terminals to hear about. Empty means all (and also doc events).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Filter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_of: Option<String>,
    #[serde(skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
}

/// A message on a subscribed connection.
#[derive(Debug, Clone)]
pub enum Notification {
    Event(Event),
    /// Every 15 s when idle, with the current seq.
    Heartbeat { seq: u64 },
    /// We fell behind: replace state with this snapshot, drop events with seq <= this.
    Missed { seq: u64, terminals: Vec<Terminal> },
    /// A notification this client doesn't know.
    Other { method: String, params: Value },
}

#[derive(Debug, Clone)]
pub struct Event {
    pub seq: u64,
    /// Absent for doc events.
    pub terminal: Option<String>,
    pub data: EventData,
}

#[derive(Debug, Clone)]
pub enum EventData {
    Created(Box<Terminal>),
    /// A kept terminal lost its program; it stays live and waits for a new shell.
    Lost(Ending),
    /// The kept terminal has a new shell.
    Restarted(Box<Terminal>),
    Keep { keep: bool },
    Finished(Ending),
    Removed,
    Status {
        status: Option<Status>,
        records: BTreeMap<String, Status>,
        reported: bool,
    },
    /// A turn ended on the word `state`.
    Turn { turn: u64, state: String },
    Title { title: String },
    Labels {
        labels: Labels,
        /// Present when the terminal newly matches the subscription's label filter.
        record: Option<Box<Terminal>>,
    },
    Size { cols: u16, rows: u16 },
    Foreground(Option<Foreground>),
    Doc { name: String, version: u64 },
    /// An event type this client doesn't know (the API only grows).
    Unknown { kind: String, data: Value },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ending {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    #[serde(default)]
    pub finished_ms: Option<u64>,
}

impl Notification {
    pub(crate) fn decode(msg: Value) -> Result<Self> {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol(format!("no method: {msg}")))?
            .to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let seq = || params.get("seq").and_then(Value::as_u64).unwrap_or(0);
        Ok(match method.as_str() {
            "event" => Notification::Event(Event::decode(&params)?),
            "heartbeat" => Notification::Heartbeat { seq: seq() },
            "missed" => Notification::Missed {
                seq: seq(),
                terminals: serde_json::from_value(
                    params.get("terminals").cloned().unwrap_or(Value::Array(vec![])),
                )?,
            },
            _ => Notification::Other { method, params },
        })
    }
}

impl Event {
    fn decode(params: &Value) -> Result<Self> {
        #[derive(Deserialize)]
        struct Raw {
            seq: u64,
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            terminal: Option<String>,
            #[serde(default)]
            data: Value,
        }
        let raw: Raw = serde_json::from_value(params.clone())?;
        let d = raw.data;
        fn field<T: serde::de::DeserializeOwned + Default>(d: &Value, k: &str) -> Result<T> {
            match d.get(k) {
                None | Some(Value::Null) => Ok(T::default()),
                Some(v) => Ok(serde_json::from_value(v.clone())?),
            }
        }
        let data = match raw.kind.as_str() {
            "created" => EventData::Created(Box::new(serde_json::from_value(d)?)),
            "restarted" => EventData::Restarted(Box::new(serde_json::from_value(d)?)),
            "lost" => EventData::Lost(serde_json::from_value(d)?),
            "finished" => EventData::Finished(serde_json::from_value(d)?),
            "removed" => EventData::Removed,
            "keep" => EventData::Keep {
                keep: field(&d, "keep")?,
            },
            "status" => EventData::Status {
                status: field(&d, "status")?,
                records: field(&d, "records")?,
                reported: field(&d, "reported")?,
            },
            "turn" => EventData::Turn {
                turn: field(&d, "turn")?,
                state: field(&d, "state")?,
            },
            "title" => EventData::Title {
                title: field(&d, "title")?,
            },
            "labels" => EventData::Labels {
                labels: field(&d, "labels")?,
                record: field::<Option<Terminal>>(&d, "record")?.map(Box::new),
            },
            "size" => EventData::Size {
                cols: field(&d, "cols")?,
                rows: field(&d, "rows")?,
            },
            "foreground" => EventData::Foreground(serde_json::from_value(d)?),
            "doc" => EventData::Doc {
                name: field(&d, "name")?,
                version: field(&d, "version")?,
            },
            _ => EventData::Unknown {
                kind: raw.kind,
                data: d,
            },
        };
        Ok(Event {
            seq: raw.seq,
            terminal: raw.terminal,
            data,
        })
    }
}

/// A live subscription on its own connection.
pub struct Subscription {
    /// The seq of the snapshot.
    pub seq: u64,
    /// Matching terminals at subscribe time.
    pub terminals: Vec<Terminal>,
    rx: mpsc::UnboundedReceiver<Notification>,
    // Keeps the connection's write half (and so the connection) open.
    _client: Client,
}

impl Subscription {
    pub(crate) async fn start(
        client: Client,
        rx: mpsc::UnboundedReceiver<Notification>,
        filter: &Filter,
    ) -> Result<Self> {
        #[derive(Deserialize)]
        struct Reply {
            seq: u64,
            terminals: Vec<Terminal>,
        }
        let reply: Reply = client
            .call("events.subscribe", serde_json::to_value(filter)?)
            .await?;
        Ok(Subscription {
            seq: reply.seq,
            terminals: reply.terminals,
            rx,
            _client: client,
        })
    }

    /// The next notification, or `None` when the connection closed.
    pub async fn next(&mut self) -> Option<Notification> {
        self.rx.recv().await
    }
}

/// What a [`Watcher`] tells you.
#[derive(Debug, Clone)]
pub enum WatchMsg {
    /// Replace everything you know with this (first connect, reconnect, or `missed`).
    Snapshot { seq: u64, terminals: Vec<Terminal> },
    Event(Event),
    /// The core went away; a new `Snapshot` follows once it's back.
    Disconnected { error: String },
}

/// A subscription that survives core restarts: it reconnects with backoff and
/// sends a fresh snapshot each time.
pub struct Watcher;

impl Watcher {
    /// Start watching. Drop the receiver to stop.
    pub fn spawn(path: PathBuf, filter: Filter) -> mpsc::Receiver<WatchMsg> {
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(watch_loop(path, filter, tx));
        rx
    }
}

/// Heartbeats come every 15 s; three missed means the line is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
const BACKOFF: [u64; 5] = [250, 500, 1000, 2000, 5000];

async fn watch_loop(path: PathBuf, filter: Filter, tx: mpsc::Sender<WatchMsg>) {
    let mut attempt = 0usize;
    loop {
        let error = match Client::subscribe_at(&path, &filter).await {
            Ok(mut sub) => {
                attempt = 0;
                let mut last_seq = sub.seq;
                let snapshot = WatchMsg::Snapshot {
                    seq: sub.seq,
                    terminals: std::mem::take(&mut sub.terminals),
                };
                if tx.send(snapshot).await.is_err() {
                    return;
                }
                loop {
                    let next = tokio::time::timeout(SILENCE_LIMIT, sub.next()).await;
                    let msg = match next {
                        Err(_) => break "no heartbeat from core".to_string(),
                        Ok(None) => break "connection closed".to_string(),
                        Ok(Some(n)) => n,
                    };
                    let out = match msg {
                        Notification::Event(e) if e.seq > last_seq => {
                            last_seq = e.seq;
                            WatchMsg::Event(e)
                        }
                        Notification::Event(_) => continue, // older than our snapshot
                        Notification::Missed { seq, terminals } => {
                            last_seq = seq;
                            WatchMsg::Snapshot { seq, terminals }
                        }
                        Notification::Heartbeat { .. } | Notification::Other { .. } => continue,
                    };
                    if tx.send(out).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => e.to_string(),
        };
        if tx.send(WatchMsg::Disconnected { error }).await.is_err() {
            return;
        }
        let delay = BACKOFF[attempt.min(BACKOFF.len() - 1)];
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(v: Value) -> EventData {
        match Notification::decode(json!({"jsonrpc":"2.0","method":"event","params":v})).unwrap() {
            Notification::Event(e) => e.data,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decodes_event_types() {
        assert!(matches!(
            ev(json!({"seq":1,"type":"status","terminal":"t1","data":{"status":{"state":"working"},"records":{},"reported":true}})),
            EventData::Status { status: Some(s), reported: true, .. } if s.state == "working"
        ));
        assert!(matches!(
            ev(json!({"seq":2,"type":"status","terminal":"t1","data":{"status":null,"records":{},"reported":true}})),
            EventData::Status { status: None, .. }
        ));
        assert!(matches!(
            ev(json!({"seq":3,"type":"foreground","terminal":"t1","data":null})),
            EventData::Foreground(None)
        ));
        assert!(matches!(
            ev(json!({"seq":4,"type":"finished","terminal":"t1","data":{"reason":"exited","exit_code":0,"signal":null,"finished_ms":5}})),
            EventData::Finished(Ending { exit_code: Some(0), .. })
        ));
        assert!(matches!(
            ev(json!({"seq":5,"type":"doc","data":{"name":"hiero","version":2}})),
            EventData::Doc { version: 2, .. }
        ));
        assert!(matches!(
            ev(json!({"seq":6,"type":"teleported","terminal":"t1","data":{}})),
            EventData::Unknown { .. }
        ));
    }

    #[test]
    fn decodes_heartbeat_and_missed() {
        let n = Notification::decode(json!({"jsonrpc":"2.0","method":"heartbeat","params":{"seq":9}}))
            .unwrap();
        assert!(matches!(n, Notification::Heartbeat { seq: 9 }));
        let n = Notification::decode(
            json!({"jsonrpc":"2.0","method":"missed","params":{"seq":10,"terminals":[]}}),
        )
        .unwrap();
        assert!(matches!(n, Notification::Missed { seq: 10, .. }));
    }
}
