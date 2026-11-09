//! Dialog state: text inputs, the new-agent form, confirmations, templates.

use crate::app::{Action, LABEL_AGENT, LABEL_PROJECT, LABEL_TASK};
use crate::git;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hc_client::{RunParams, TemplateInfo};
use std::path::{Path, PathBuf};

// ---- text input -------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    /// Cursor position in chars.
    cursor: usize,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        TextInput { text, cursor }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.text.len())
    }

    /// Insert pasted text at the cursor (newlines become spaces).
    pub fn insert_str(&mut self, text: &str) {
        let clean: String = text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
        let b = self.byte_at(self.cursor);
        self.text.insert_str(b, &clean);
        self.cursor += clean.chars().count();
    }

    /// Edit on a key; false when the key isn't for the input.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char('u') if ctrl => {
                let b = self.byte_at(self.cursor);
                self.text.replace_range(..b, "");
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                let (a, b) = (self.byte_at(start), self.byte_at(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char(_) if ctrl => return false,
            KeyCode::Char(c) => {
                let b = self.byte_at(self.cursor);
                self.text.insert(b, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let b = self.byte_at(self.cursor - 1);
                self.text.remove(b);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let b = self.byte_at(self.cursor);
                self.text.remove(b);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

pub enum FormResult {
    Continue,
    Cancel,
    Submit,
}

// ---- new agent --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentKind {
    /// An hc template: started by name, task appended by hc after readiness.
    Template(String),
    /// A command on PATH; the task goes in as its last argument.
    Command(String),
    /// The machine's shell; the task is typed in as a command.
    Shell,
}

#[derive(Debug, Clone)]
pub struct AgentChoice {
    pub label: String,
    pub detail: String,
    pub kind: AgentKind,
}

/// Interactive coding agents Hiero looks for on PATH.
const KNOWN_AGENTS: &[&str] = &["claude", "codex", "pi", "gemini", "opencode", "aider", "goose", "amp", "cursor-agent"];

pub fn agent_choices(templates: &[TemplateInfo]) -> Vec<AgentChoice> {
    let mut out: Vec<AgentChoice> = templates
        .iter()
        .map(|t| AgentChoice {
            label: t.name.clone(),
            detail: t.description.clone().unwrap_or_else(|| "template".into()),
            kind: AgentKind::Template(t.name.clone()),
        })
        .collect();
    for a in KNOWN_AGENTS {
        if on_path(a) {
            out.push(AgentChoice {
                label: a.to_string(),
                detail: format!("runs `{a} <task>`"),
                kind: AgentKind::Command(a.to_string()),
            });
        }
    }
    out.push(AgentChoice {
        label: "shell".into(),
        detail: "your shell; the task runs as a command".into(),
        kind: AgentKind::Shell,
    });
    out
}

fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnField {
    Agent,
    Folder,
    Task,
    Name,
}

const FIELDS: [SpawnField; 4] = [SpawnField::Agent, SpawnField::Folder, SpawnField::Task, SpawnField::Name];

pub struct SpawnForm {
    pub field: SpawnField,
    pub agents: Vec<AgentChoice>,
    pub agent: usize,
    pub folder: TextInput,
    pub task: TextInput,
    pub name: TextInput,
    pub error: Option<String>,
}

/// What to do once the form is valid.
pub struct SpawnPlan {
    pub params: RunParams,
    /// Typed into the terminal after it starts (shell tasks).
    pub send_after: Option<String>,
}

impl SpawnForm {
    pub fn new(agents: Vec<AgentChoice>, folder: String, preselect: Option<&str>) -> Self {
        let agent = preselect
            .and_then(|p| agents.iter().position(|a| a.label == p))
            .or_else(|| agents.iter().position(|a| a.label == "claude"))
            .unwrap_or(0);
        SpawnForm {
            field: if preselect.is_some() { SpawnField::Task } else { SpawnField::Agent },
            agents,
            agent,
            folder: TextInput::new(folder),
            task: TextInput::default(),
            name: TextInput::default(),
            error: None,
        }
    }

    fn step(&mut self, delta: isize) {
        let i = FIELDS.iter().position(|f| *f == self.field).unwrap() as isize;
        self.field = FIELDS[(i + delta).rem_euclid(FIELDS.len() as isize) as usize];
    }

    pub fn on_key(&mut self, key: KeyEvent) -> FormResult {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormResult::Cancel,
            KeyCode::Char('s') if ctrl => return FormResult::Submit,
            KeyCode::Enter if self.field == SpawnField::Name => return FormResult::Submit,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.step(1),
            KeyCode::BackTab | KeyCode::Up => self.step(-1),
            _ => {
                self.error = None;
                match self.field {
                    SpawnField::Agent => {
                        let n = self.agents.len().max(1);
                        match key.code {
                            KeyCode::Left | KeyCode::Char('h') => self.agent = (self.agent + n - 1) % n,
                            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => self.agent = (self.agent + 1) % n,
                            _ => {}
                        }
                    }
                    SpawnField::Folder => {
                        self.folder.handle(key);
                    }
                    SpawnField::Task => {
                        self.task.handle(key);
                    }
                    SpawnField::Name => {
                        self.name.handle(key);
                    }
                }
            }
        }
        FormResult::Continue
    }

    pub fn paste(&mut self, text: &str) {
        match self.field {
            SpawnField::Agent => {}
            SpawnField::Folder => self.folder.insert_str(text.trim()),
            SpawnField::Task => self.task.insert_str(text),
            SpawnField::Name => self.name.insert_str(text.trim()),
        }
    }

    pub fn selected_agent(&self) -> Option<&AgentChoice> {
        self.agents.get(self.agent)
    }

    /// Validate and turn the form into a run request.
    pub fn plan(&self) -> Result<SpawnPlan, String> {
        let agent = self.selected_agent().ok_or("no agent to start")?;
        let folder = expand_home(self.folder.value().trim());
        if !folder.is_dir() {
            return Err(format!("no such folder: {}", folder.display()));
        }
        let task = self.task.value().trim().to_string();
        let name = self.name.value().trim();

        let mut params = RunParams::new().cwd(folder.to_string_lossy());
        let mut send_after = None;
        match &agent.kind {
            AgentKind::Template(t) => {
                let mut argv = vec![t.clone()];
                if !task.is_empty() {
                    argv.push(task.clone());
                }
                params = params.argv(argv);
            }
            AgentKind::Command(cmd) => {
                let mut argv = shell_words::split(cmd).map_err(|e| format!("bad command: {e}"))?;
                if !task.is_empty() {
                    argv.push(task.clone());
                }
                params = params.argv(argv);
            }
            AgentKind::Shell => {
                if !task.is_empty() {
                    send_after = Some(task.clone());
                }
            }
        }
        if !name.is_empty() {
            params = params.name(name);
        }
        let project = git::probe(&folder)
            .map(|g| g.name())
            .or_else(|| folder.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        params = params.label(LABEL_PROJECT, project).label(LABEL_AGENT, agent.label.clone());
        if !task.is_empty() {
            params = params.label(LABEL_TASK, task.chars().take(200).collect::<String>());
        }
        Ok(SpawnPlan { params, send_after })
    }
}

pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix('~'), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(p),
    }
}

// ---- single-line prompts and confirmations ----------------------------------

pub enum InputPurpose {
    Send(String),
    Rename(String),
    NewTemplate,
}

pub struct InputDialog {
    pub title: String,
    pub hint: String,
    pub input: TextInput,
    pub purpose: InputPurpose,
    /// Send even when the agent is blocked (it may be answering its dialog).
    pub force: bool,
}

pub struct Confirm {
    pub message: String,
    pub action: Action,
}

// ---- templates --------------------------------------------------------------

pub struct TemplatesView {
    pub items: Vec<TemplateInfo>,
    pub selected: usize,
}

impl TemplatesView {
    pub fn current(&self) -> Option<&TemplateInfo> {
        self.items.get(self.selected)
    }
}

/// `$XDG_CONFIG_HOME/herdr-core/templates` or `~/.config/herdr-core/templates`.
pub fn templates_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home("~/.config"));
    base.join("herdr-core").join("templates")
}

pub fn template_path(name: &str) -> PathBuf {
    templates_dir().join(format!("{name}.md"))
}

/// A starting point for a new template.
pub fn template_skeleton(name: &str) -> String {
    let cmd = KNOWN_AGENTS.iter().find(|a| on_path(a)).copied().unwrap_or("claude");
    format!(
        "---\ndescription: {name} agent. Say what it is for.\ncommand: {cmd}\n---\n\
         You are the {name} agent.\n\
         Describe its role, what it may and may not do, and how to report back.\n"
    )
}

pub fn valid_template_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !Path::new(name).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    #[test]
    fn text_input_edits() {
        let mut t = TextInput::new("hello world");
        t.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.value(), "hello ");
        t.handle(key(KeyCode::Home));
        t.handle(key(KeyCode::Char('¡')));
        assert_eq!(t.value(), "¡hello ");
        t.handle(key(KeyCode::Backspace));
        t.handle(key(KeyCode::Delete));
        assert_eq!(t.value(), "ello ");
    }

    fn form(kind: AgentKind) -> SpawnForm {
        let agents = vec![AgentChoice { label: "x".into(), detail: String::new(), kind }];
        let mut f = SpawnForm::new(agents, env!("CARGO_MANIFEST_DIR").into(), None);
        f.task = TextInput::new("fix the bug");
        f
    }

    #[test]
    fn plans_each_agent_kind() {
        let p = form(AgentKind::Command("claude --model opus".into())).plan().unwrap();
        assert_eq!(p.params.argv, ["claude", "--model", "opus", "fix the bug"]);
        assert_eq!(p.params.labels.get(LABEL_TASK).unwrap(), "fix the bug");
        assert_eq!(p.params.labels.get(LABEL_PROJECT).unwrap(), "hiero");
        assert!(p.send_after.is_none());

        let p = form(AgentKind::Template("reviewer".into())).plan().unwrap();
        assert_eq!(p.params.argv, ["reviewer", "fix the bug"]);

        let p = form(AgentKind::Shell).plan().unwrap();
        assert!(p.params.argv.is_empty());
        assert_eq!(p.send_after.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn rejects_missing_folder() {
        let mut f = form(AgentKind::Shell);
        f.folder = TextInput::new("/definitely/not/here");
        assert!(f.plan().is_err());
    }

    #[test]
    fn template_names() {
        assert!(valid_template_name("reviewer-2"));
        assert!(!valid_template_name("../evil"));
        assert!(!valid_template_name(""));
    }
}
