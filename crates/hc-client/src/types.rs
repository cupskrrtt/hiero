//! Typed records from the hc API (`hc schema`). Unknown fields are ignored and
//! most fields default, because the API only grows.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Labels = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloResult {
    pub generation: u32,
    pub version: String,
    pub core: CoreInfo,
    pub framed: bool,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatusResult {
    pub core: CoreInfo,
    pub version: String,
    pub pid: u32,
    pub uptime_ms: u64,
    pub live: u64,
    pub finished: u64,
    #[serde(default)]
    pub installed_by: Option<String>,
}

/// `live` or `finished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    #[default]
    Live,
    Finished,
    #[serde(other)]
    Unknown,
}

/// What a program reported about itself through OSC 7501.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// working, idle, done, blocked, error, … (free word; see [`AgentState`]).
    pub state: String,
    #[serde(default)]
    pub app: String,
    /// permission, question or auth (blocked only).
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub msg: String,
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub title: String,
}

/// A coarse reading of [`Status::state`] for display and sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentState {
    Blocked,
    Error,
    Done,
    Working,
    Idle,
    /// The program never reported, or its status was cleared.
    Unknown,
}

impl AgentState {
    pub fn from_word(word: &str) -> Self {
        match word {
            "blocked" => Self::Blocked,
            "error" => Self::Error,
            "done" => Self::Done,
            "working" => Self::Working,
            "idle" => Self::Idle,
            _ => Self::Unknown,
        }
    }

    /// Whether a person should look at it.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Blocked | Self::Error | Self::Done)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Foreground {
    pub pid: i32,
    pub name: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: String,
}

/// One terminal, live or finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    /// `t7`, or `machine/t7` for a remote one.
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub state: TerminalState,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub finished_ms: Option<u64>,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub status: Option<Status>,
    #[serde(default)]
    pub status_ms: Option<u64>,
    #[serde(default)]
    pub records: BTreeMap<String, Status>,
    #[serde(default)]
    pub reported: bool,
    #[serde(default)]
    pub foreground: Option<Foreground>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub keep: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
}

impl Terminal {
    /// Name if it has one, else the address.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.address)
    }

    pub fn agent_state(&self) -> AgentState {
        self.status
            .as_ref()
            .map(|s| AgentState::from_word(&s.state))
            .unwrap_or(AgentState::Unknown)
    }

    pub fn is_live(&self) -> bool {
        self.state == TerminalState::Live
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListResult {
    pub terminals: Vec<Terminal>,
    #[serde(default)]
    pub hidden_finished: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadResult {
    pub text: String,
    pub complete: bool,
    pub how: String,
    #[serde(default)]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendResult {
    pub turn: u64,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub started: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeysResult {
    pub turn: u64,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterruptResult {
    pub outcome: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoveResult {
    pub removed: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocGetResult {
    pub value: serde_json::Value,
    pub version: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocSetResult {
    pub version: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TemplateInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TemplatesResult {
    pub templates: Vec<TemplateInfo>,
}

/// Parameters for `terminal.run`. Build with `RunParams::new()` and the setters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunParams {
    /// Program and arguments, or a template name followed by its task.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    /// A shell command line instead of argv.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    /// Another machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// Hiero is not an agent: started terminals have no parent by default.
    pub no_parent: bool,
}

impl RunParams {
    pub fn new() -> Self {
        Self {
            no_parent: true,
            ..Default::default()
        }
    }
    pub fn argv<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv = argv.into_iter().map(Into::into).collect();
        self
    }
    pub fn line(mut self, line: impl Into<String>) -> Self {
        self.line = Some(line.into());
        self
    }
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    pub fn label(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.labels.insert(k.into(), v.into());
        self
    }
    pub fn keep(mut self, keep: bool) -> Self {
        self.keep = keep;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_tolerates_minimal_and_extra_fields() {
        let t: Terminal = serde_json::from_str(
            r#"{"address":"t1","labels":{},"argv":["bash"],"cwd":"/","state":"live",
                "created_ms":1,"screen":"","cols":80,"rows":24,"future_field":true}"#,
        )
        .unwrap();
        assert_eq!(t.display_name(), "t1");
        assert_eq!(t.agent_state(), AgentState::Unknown);
        assert!(t.is_live());
    }

    #[test]
    fn run_params_serialize_compactly() {
        let p = RunParams::new().argv(["bash"]).name("x").label("hiero.project", "p");
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"argv":["bash"],"name":"x","labels":{"hiero.project":"p"},"no_parent":true})
        );
    }
}
