//! Pipelines: stages of agents (or commands) that run one after another,
//! each stage possibly fanned out to several parallel members.
//!
//! ```toml
//! description = "Plan, build in parallel, review"
//!
//! [[stage]]
//! name = "plan"
//! agent = "claude"
//! prompt = "Plan {task} in PLAN.md as {count} independent parts."
//!
//! [[stage]]
//! name = "build"
//! agent = "claude"
//! count = 3          # parallel members
//! worktree = true    # each member is a task on its own branch
//! prompt = "Do part {n} of {dirs}/PLAN.md."
//!
//! [[stage]]
//! name = "check"
//! command = "cargo test"   # a command instead of an agent; exit 0 = success
//! ```

use crate::forms::{AgentKind, expand_home};
use hc_client::{AgentState, Terminal, TerminalSet};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageSpec {
    pub name: String,
    /// claude, codex, a template name, or "shell".
    #[serde(default)]
    pub agent: Option<String>,
    /// The agent's first message.
    #[serde(default)]
    pub prompt: Option<String>,
    /// A command line instead of an agent; the stage passes when it exits 0.
    #[serde(default)]
    pub command: Option<String>,
    /// Parallel members ("{count}" in prompts; default 1).
    #[serde(default)]
    pub count: Option<u32>,
    /// Each member works in its own git worktree (a task).
    #[serde(default)]
    pub worktree: bool,
}

impl StageSpec {
    pub fn members(&self) -> u32 {
        self.count.unwrap_or(1).clamp(1, 16)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineSpec {
    #[serde(skip)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "stage", default)]
    pub stages: Vec<StageSpec>,
}

impl PipelineSpec {
    pub fn parse(name: &str, text: &str) -> Result<Self, String> {
        let mut p: PipelineSpec = toml::from_str(text).map_err(|e| format!("{name}: {e}"))?;
        p.name = name.to_string();
        if p.stages.is_empty() {
            return Err(format!("{name}: no [[stage]] entries"));
        }
        for s in &p.stages {
            match (&s.agent, &s.command) {
                (Some(_), Some(_)) => return Err(format!("{name}/{}: use agent or command, not both", s.name)),
                (None, None) => return Err(format!("{name}/{}: needs agent or command", s.name)),
                (None, Some(_)) if s.worktree => {
                    return Err(format!("{name}/{}: worktree needs an agent", s.name));
                }
                _ => {}
            }
        }
        Ok(p)
    }
}

pub const BUILTIN_NAME: &str = "plan-build-review";
pub const BUILTIN: &str = r#"description = "Plan, build the parts in parallel worktrees, then review"

[[stage]]
name = "plan"
agent = "claude"
prompt = "Plan this task: {task}. Write the plan to PLAN.md in this folder as exactly 2 independent parts, each under a heading '## Part N', so two people could do them in parallel without touching the same files. Do not implement anything."

[[stage]]
name = "build"
agent = "claude"
count = 2
worktree = true
prompt = "Read {dirs}/PLAN.md and implement only Part {n} of {count} in this folder. Commit nothing; just make the change. Overall task: {task}"

[[stage]]
name = "review"
agent = "claude"
prompt = "Review the work on branches {branches} (folders: {dirs}) against the plan in PLAN.md and the task: {task}. Report problems with file paths; do not edit files."
"#;

/// `~/.config/hiero/pipelines`.
pub fn pipelines_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home("~/.config"));
    base.join("hiero").join("pipelines")
}

/// The built-in pipeline plus every `*.toml` in the pipelines folder.
/// Broken files come back as errors so the person can see why.
pub fn load_all() -> (Vec<PipelineSpec>, Vec<String>) {
    let mut out = vec![PipelineSpec::parse(BUILTIN_NAME, BUILTIN).expect("built-in pipeline parses")];
    let mut errors = Vec::new();
    if let Ok(dir) = std::fs::read_dir(pipelines_dir()) {
        let mut files: Vec<_> = dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "toml")).collect();
        files.sort();
        for path in files {
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| PipelineSpec::parse(&name, &t)) {
                Ok(p) => {
                    out.retain(|q| q.name != p.name); // a file can override the built-in
                    out.push(p);
                }
                Err(e) => errors.push(e),
            }
        }
    }
    (out, errors)
}

// ---- runs -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Done,
    Failed,
    Stopped,
}

impl RunStatus {
    pub fn word(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
            RunStatus::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub terminal: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    pub dir: String,
    /// The turn counter when it started; done once a turn past this ends.
    #[serde(default)]
    pub start_turn: u64,
    /// A command, not an agent: judged by its exit code.
    #[serde(default)]
    pub command: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub pipeline: String,
    pub task: String,
    pub folder: String,
    /// The stages as they were when the run began.
    pub stages: Vec<StageSpec>,
    /// Index of the current stage.
    pub stage: usize,
    #[serde(default)]
    pub members: Vec<Member>,
    /// Members of finished stages, in order.
    #[serde(default)]
    pub history: Vec<Vec<Member>>,
    pub status: RunStatus,
    /// Which Hiero advances this run (only one may).
    pub driver: String,
    pub started_ms: u64,
    #[serde(default)]
    pub note: String,
}

impl Run {
    pub fn current(&self) -> Option<&StageSpec> {
        self.stages.get(self.stage)
    }

    /// The first terminal of the first stage: parent of everything else.
    pub fn root(&self) -> Option<&str> {
        self.history.first().and_then(|s| s.first()).or(self.members.first()).map(|m| m.terminal.as_str())
    }

    pub fn previous(&self) -> &[Member] {
        self.history.last().map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn progress(&self) -> String {
        match self.current() {
            Some(s) if self.status == RunStatus::Running => {
                format!("stage {}/{} {}", self.stage + 1, self.stages.len(), s.name)
            }
            _ => self.status.word().to_string(),
        }
    }
}

/// How a member is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    Busy,
    Finished,
    Failed,
}

pub fn member_state(m: &Member, t: Option<&Terminal>) -> MemberState {
    let Some(t) = t else {
        return MemberState::Finished; // removed: nothing left to wait for
    };
    if !t.is_live() {
        return if m.command && t.exit_code != Some(0) && t.reason.as_deref() != Some("killed") {
            MemberState::Failed
        } else {
            MemberState::Finished
        };
    }
    if m.command {
        return MemberState::Busy;
    }
    match t.agent_state() {
        // A turn ended past the start: its work is in.
        AgentState::Done | AgentState::Idle if t.turn > m.start_turn => MemberState::Finished,
        _ => MemberState::Busy,
    }
}

/// What the driver should do for a run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Wait,
    /// Start the stage at this index.
    Start(usize),
    Finish,
    Fail(String),
}

pub fn next_step(run: &Run, set: &TerminalSet) -> Step {
    if run.status != RunStatus::Running {
        return Step::Wait;
    }
    if run.members.is_empty() {
        return Step::Start(run.stage);
    }
    let mut busy = false;
    for m in &run.members {
        match member_state(m, set.get(&m.terminal)) {
            MemberState::Busy => busy = true,
            MemberState::Failed => {
                let stage = run.current().map(|s| s.name.as_str()).unwrap_or("?");
                return Step::Fail(format!("{stage}: {} failed", m.terminal));
            }
            MemberState::Finished => {}
        }
    }
    if busy {
        Step::Wait
    } else if run.stage + 1 < run.stages.len() {
        Step::Start(run.stage + 1)
    } else {
        Step::Finish
    }
}

/// Fill `{task}`, `{n}`, `{count}`, `{branches}`, `{dirs}`, `{folder}`, `{run}`.
pub fn fill(template: &str, run: &Run, n: u32, count: u32) -> String {
    let prev = run.previous();
    let branches: Vec<&str> = prev.iter().filter_map(|m| m.branch.as_deref()).collect();
    let dirs: Vec<&str> = prev.iter().map(|m| m.dir.as_str()).collect();
    template
        .replace("{task}", &run.task)
        .replace("{n}", &n.to_string())
        .replace("{count}", &count.to_string())
        .replace("{branches}", &if branches.is_empty() { "(none)".into() } else { branches.join(" ") })
        .replace("{dirs}", &if dirs.is_empty() { run.folder.clone() } else { dirs.join(" ") })
        .replace("{folder}", &run.folder)
        .replace("{run}", &run.id)
}

/// Turn a stage's agent name into how to start it.
pub fn resolve_agent(name: &str, templates: &[hc_client::TemplateInfo]) -> AgentKind {
    if name == "shell" {
        AgentKind::Shell
    } else if templates.iter().any(|t| t.name == name) {
        AgentKind::Template(name.to_string())
    } else {
        AgentKind::Command(crate::config::command_for(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(addr: &str, live: bool, state: Option<&str>, turn: u64, exit: Option<i32>) -> Terminal {
        let mut t: Terminal = serde_json::from_value(serde_json::json!({
            "address": addr, "labels": {}, "argv": [], "cwd": "/", "state": if live {"live"} else {"finished"},
            "created_ms": 1, "screen": "", "cols": 80, "rows": 24, "turn": turn
        }))
        .unwrap();
        t.exit_code = exit;
        t.status = state.map(|s| hc_client::Status {
            state: s.into(),
            app: String::new(),
            kind: None,
            msg: String::new(),
            progress: None,
            title: String::new(),
        });
        t
    }

    fn run_with(stages: usize, members: Vec<Member>) -> Run {
        let spec = PipelineSpec::parse("x", BUILTIN).unwrap();
        Run {
            id: "r1".into(),
            pipeline: "x".into(),
            task: "add login".into(),
            folder: "/repo".into(),
            stages: spec.stages.into_iter().take(stages).collect(),
            stage: 0,
            members,
            history: vec![],
            status: RunStatus::Running,
            driver: "me".into(),
            started_ms: 0,
            note: String::new(),
        }
    }

    fn member(t: &str, command: bool) -> Member {
        Member { terminal: t.into(), task_id: None, branch: None, dir: "/repo".into(), start_turn: 0, command }
    }

    #[test]
    fn builtin_and_validation() {
        let p = PipelineSpec::parse("x", BUILTIN).unwrap();
        assert_eq!(p.stages.len(), 3);
        assert_eq!(p.stages[1].members(), 2);
        assert!(PipelineSpec::parse("bad", "[[stage]]\nname='a'").is_err());
        assert!(PipelineSpec::parse("bad", "[[stage]]\nname='a'\nagent='x'\ncommand='y'").is_err());
        assert!(PipelineSpec::parse("bad", "description='only'").is_err());
    }

    #[test]
    fn steps_through_stages() {
        let mut set = TerminalSet::new();
        let mut run = run_with(2, vec![]);
        assert_eq!(next_step(&run, &set), Step::Start(0));

        run.members = vec![member("t1", false)];
        set.reset(1, vec![term("t1", true, Some("working"), 0, None)]);
        assert_eq!(next_step(&run, &set), Step::Wait);

        set.reset(2, vec![term("t1", true, Some("done"), 1, None)]);
        assert_eq!(next_step(&run, &set), Step::Start(1));

        run.history.push(run.members.clone());
        run.stage = 1;
        run.members = vec![member("t2", false), member("t3", true)];
        set.reset(3, vec![term("t2", true, Some("idle"), 1, None), term("t3", false, None, 0, Some(0))]);
        assert_eq!(next_step(&run, &set), Step::Finish);

        set.reset(4, vec![term("t2", true, Some("idle"), 1, None), term("t3", false, None, 0, Some(2))]);
        assert!(matches!(next_step(&run, &set), Step::Fail(_)));
    }

    #[test]
    fn blocked_agents_hold_the_stage() {
        let mut set = TerminalSet::new();
        let run = run_with(1, vec![member("t1", false)]);
        set.reset(1, vec![term("t1", true, Some("blocked"), 0, None)]);
        assert_eq!(next_step(&run, &set), Step::Wait);
    }

    #[test]
    fn fills_placeholders() {
        let mut run = run_with(2, vec![]);
        run.history.push(vec![Member { branch: Some("hiero/a".into()), dir: "/w/a".into(), ..member("t1", false) }]);
        let s = fill("{task}|{n}/{count}|{branches}|{dirs}|{folder}", &run, 2, 3);
        assert_eq!(s, "add login|2/3|hiero/a|/w/a|/repo");
    }
}
