//! Task records, kept in herdr-core's document `hiero` so they survive
//! restarts and every Hiero sees the same list.

use hc_client::{Client, ErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const DOC_NAME: &str = "hiero";
const MAX_TIMELINE: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Agent working (or waiting) in its worktree.
    Running,
    /// Merged into the base branch; worktree removed.
    Merged,
    /// Pushed with a pull request open; worktree kept.
    Pr,
    /// Left as is for later.
    Kept,
    /// Thrown away; worktree and branch removed.
    Discarded,
}

impl TaskStatus {
    pub fn word(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Merged => "merged",
            TaskStatus::Pr => "pr open",
            TaskStatus::Kept => "kept",
            TaskStatus::Discarded => "discarded",
        }
    }

    /// The worktree still exists.
    pub fn has_worktree(self) -> bool {
        matches!(self, TaskStatus::Running | TaskStatus::Pr | TaskStatus::Kept)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub ms: u64,
    pub what: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub agent: String,
    /// Main working tree.
    pub repo: String,
    pub base_branch: String,
    #[serde(default)]
    pub base_commit: String,
    pub branch: String,
    pub worktree: String,
    /// The terminal running the agent.
    #[serde(default)]
    pub terminal: Option<String>,
    pub created_ms: u64,
    pub status: TaskStatus,
    #[serde(default)]
    pub timeline: Vec<TimelineEntry>,
    #[serde(default)]
    pub pr_url: Option<String>,
}

/// The whole document. Unknown keys are kept as they are.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Doc {
    #[serde(default)]
    pub tasks: BTreeMap<String, Task>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Doc {
    pub fn from_value(v: &Value) -> Doc {
        if v.is_null() {
            return Doc::default();
        }
        serde_json::from_value(v.clone()).unwrap_or_else(|e| {
            tracing::warn!("doc {DOC_NAME} unreadable, starting empty: {e}");
            Doc::default()
        })
    }

    /// The task whose agent runs in `terminal`.
    pub fn by_terminal(&self, terminal: &str) -> Option<&Task> {
        self.tasks.values().find(|t| t.terminal.as_deref() == Some(terminal))
    }
}

/// One change to the document.
#[derive(Debug, Clone)]
pub enum TaskOp {
    Insert(Box<Task>),
    Status { id: String, status: TaskStatus, note: String },
    Note { id: String, what: String },
    SetPr { id: String, url: String },
    Forget(String),
}

pub fn apply(doc: &mut Doc, op: &TaskOp, now: u64) {
    let note = |id: &str, what: &str, doc: &mut Doc| {
        if let Some(t) = doc.tasks.get_mut(id) {
            t.timeline.push(TimelineEntry { ms: now, what: what.to_string() });
            let extra = t.timeline.len().saturating_sub(MAX_TIMELINE);
            t.timeline.drain(..extra);
        }
    };
    match op {
        TaskOp::Insert(t) => {
            doc.tasks.insert(t.id.clone(), (**t).clone());
            note(&t.id, "started", doc);
        }
        TaskOp::Status { id, status, note: what } => {
            if let Some(t) = doc.tasks.get_mut(id) {
                t.status = *status;
            }
            let what = if what.is_empty() { status.word().to_string() } else { what.clone() };
            note(id, &what, doc);
        }
        TaskOp::Note { id, what } => note(id, what, doc),
        TaskOp::SetPr { id, url } => {
            if let Some(t) = doc.tasks.get_mut(id) {
                t.pr_url = Some(url.clone());
            }
        }
        TaskOp::Forget(id) => {
            doc.tasks.remove(id);
        }
    }
}

/// Read-modify-write with retries when another writer got there first.
pub async fn update(c: &Client, ops: &[TaskOp]) -> Result<Doc, String> {
    for _ in 0..5 {
        let cur = c.doc_get(DOC_NAME).await.map_err(|e| e.to_string())?;
        let mut doc = Doc::from_value(&cur.value);
        let now = crate::app::now_ms();
        for op in ops {
            apply(&mut doc, op, now);
        }
        let value = serde_json::to_value(&doc).map_err(|e| e.to_string())?;
        match c.doc_set(DOC_NAME, &value, cur.version).await {
            Ok(_) => return Ok(doc),
            Err(e) if e.is_kind(ErrorKind::Conflict) => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("the task list kept changing underneath; try again".into())
}

pub async fn load(c: &Client) -> Result<Doc, String> {
    let cur = c.doc_get(DOC_NAME).await.map_err(|e| e.to_string())?;
    Ok(Doc::from_value(&cur.value))
}

/// A short unique id: slug plus time.
pub fn new_id(slug: &str, now: u64) -> String {
    format!("{slug}-{:x}", now % 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            title: "Fix it".into(),
            prompt: "fix it".into(),
            agent: "claude".into(),
            repo: "/r".into(),
            base_branch: "main".into(),
            base_commit: "abc".into(),
            branch: format!("hiero/{id}"),
            worktree: format!("/r.hiero/{id}"),
            terminal: Some("t1".into()),
            created_ms: 1,
            status: TaskStatus::Running,
            timeline: vec![],
            pr_url: None,
        }
    }

    #[test]
    fn ops_and_round_trip() {
        let mut doc = Doc::from_value(&serde_json::json!({"other": 1}));
        apply(&mut doc, &TaskOp::Insert(Box::new(task("a"))), 10);
        apply(&mut doc, &TaskOp::Note { id: "a".into(), what: "turn 1 ended: done".into() }, 11);
        apply(&mut doc, &TaskOp::Status { id: "a".into(), status: TaskStatus::Merged, note: String::new() }, 12);
        let t = &doc.tasks["a"];
        assert_eq!(t.status, TaskStatus::Merged);
        let whats: Vec<_> = t.timeline.iter().map(|e| e.what.as_str()).collect();
        assert_eq!(whats, ["started", "turn 1 ended: done", "merged"]);
        assert_eq!(doc.by_terminal("t1").unwrap().id, "a");

        let v = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["other"], 1, "unknown keys survive");
        let back = Doc::from_value(&v);
        assert_eq!(back.tasks["a"], doc.tasks["a"]);

        apply(&mut doc, &TaskOp::Forget("a".into()), 13);
        assert!(doc.tasks.is_empty());
    }

    #[test]
    fn timeline_is_capped() {
        let mut doc = Doc::default();
        apply(&mut doc, &TaskOp::Insert(Box::new(task("a"))), 0);
        for i in 0..300 {
            apply(&mut doc, &TaskOp::Note { id: "a".into(), what: i.to_string() }, i);
        }
        assert_eq!(doc.tasks["a"].timeline.len(), MAX_TIMELINE);
        assert_eq!(doc.tasks["a"].timeline.last().unwrap().what, "299");
    }
}
