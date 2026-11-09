//! An up-to-date picture of all terminals, built from a snapshot plus events,
//! following the apply rules in herdr-core's API docs.

use crate::events::{Event, EventData, WatchMsg};
use crate::types::{Terminal, TerminalState};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct TerminalSet {
    pub seq: u64,
    /// By address.
    pub terminals: BTreeMap<String, Terminal>,
    /// False between a disconnect and the next snapshot.
    pub connected: bool,
}

impl TerminalSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply anything a [`crate::Watcher`] sends.
    pub fn apply_watch(&mut self, msg: &WatchMsg) {
        match msg {
            WatchMsg::Snapshot { seq, terminals } => self.reset(*seq, terminals.clone()),
            WatchMsg::Event(e) => self.apply(e),
            WatchMsg::Disconnected { .. } => self.connected = false,
        }
    }

    pub fn reset(&mut self, seq: u64, terminals: Vec<Terminal>) {
        self.seq = seq;
        self.connected = true;
        self.terminals = terminals
            .into_iter()
            .map(|t| (t.address.clone(), t))
            .collect();
    }

    pub fn apply(&mut self, e: &Event) {
        if e.seq <= self.seq && self.seq != 0 {
            return;
        }
        self.seq = e.seq;
        let Some(addr) = e.terminal.as_deref() else {
            return; // doc events
        };
        match &e.data {
            EventData::Created(t) | EventData::Restarted(t) => {
                self.terminals.insert(addr.to_string(), (**t).clone());
            }
            EventData::Removed => {
                self.terminals.remove(addr);
            }
            EventData::Labels {
                record: Some(t), ..
            } => {
                self.terminals.insert(addr.to_string(), (**t).clone());
            }
            data => {
                let Some(t) = self.terminals.get_mut(addr) else {
                    return;
                };
                match data {
                    EventData::Lost(end) => {
                        t.reason = end.reason.clone();
                        t.exit_code = end.exit_code;
                        t.signal = end.signal;
                        t.finished_ms = None;
                        t.pid = None;
                        t.foreground = None;
                    }
                    EventData::Finished(end) => {
                        t.state = TerminalState::Finished;
                        t.reason = end.reason.clone();
                        t.exit_code = end.exit_code;
                        t.signal = end.signal;
                        t.finished_ms = end.finished_ms;
                        t.foreground = None;
                    }
                    EventData::Keep { keep } => t.keep = *keep,
                    EventData::Status {
                        status,
                        records,
                        reported,
                    } => {
                        t.status = status.clone();
                        t.records = records.clone();
                        t.reported = *reported;
                    }
                    EventData::Turn { turn, .. } => t.turn = *turn,
                    EventData::Title { title } => t.title = title.clone(),
                    EventData::Labels { labels, .. } => t.labels = labels.clone(),
                    EventData::Size { cols, rows } => {
                        t.cols = *cols;
                        t.rows = *rows;
                    }
                    EventData::Foreground(fg) => t.foreground = fg.clone(),
                    _ => {}
                }
            }
        }
    }

    pub fn get(&self, address: &str) -> Option<&Terminal> {
        self.terminals.get(address)
    }

    pub fn len(&self) -> usize {
        self.terminals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terminals.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Ending;
    use crate::types::{AgentState, Status};

    fn term(addr: &str) -> Terminal {
        serde_json::from_value(serde_json::json!({
            "address": addr, "labels": {}, "argv": ["sh"], "cwd": "/", "state": "live",
            "created_ms": 1, "screen": "", "cols": 80, "rows": 24
        }))
        .unwrap()
    }

    fn event(seq: u64, addr: &str, data: EventData) -> Event {
        Event {
            seq,
            terminal: Some(addr.into()),
            data,
        }
    }

    #[test]
    fn applies_lifecycle() {
        let mut s = TerminalSet::new();
        s.reset(10, vec![term("t1")]);
        s.apply(&event(11, "t2", EventData::Created(Box::new(term("t2")))));
        s.apply(&event(
            12,
            "t2",
            EventData::Status {
                status: Some(Status {
                    state: "blocked".into(),
                    app: String::new(),
                    kind: Some("permission".into()),
                    msg: String::new(),
                    progress: None,
                    title: String::new(),
                }),
                records: Default::default(),
                reported: true,
            },
        ));
        assert_eq!(s.get("t2").unwrap().agent_state(), AgentState::Blocked);

        s.apply(&event(
            13,
            "t1",
            EventData::Finished(Ending {
                reason: Some("exited".into()),
                exit_code: Some(0),
                signal: None,
                finished_ms: Some(99),
            }),
        ));
        assert!(!s.get("t1").unwrap().is_live());

        s.apply(&event(14, "t1", EventData::Removed));
        assert!(s.get("t1").is_none());

        // stale events are ignored
        s.apply(&event(12, "t9", EventData::Created(Box::new(term("t9")))));
        assert!(s.get("t9").is_none());
        assert_eq!(s.len(), 1);
    }
}
