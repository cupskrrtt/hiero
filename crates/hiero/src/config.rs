//! `~/.config/hiero/config.toml`, plus per-viewer UI state in
//! `~/.local/state/hiero/ui.json`. Everything has a default; a missing or
//! partly filled file is fine.

use crate::forms::expand_home;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub board: Board,
    pub workspace: WorkspaceCfg,
    pub notify: Notify,
    /// Per-agent launch settings, by agent name (`claude`, `pi`, …).
    pub agents: BTreeMap<String, AgentConfig>,
    /// Remap board keys: action = "key".
    pub keys: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Board {
    /// project, tree or flat.
    pub grouping: String,
    pub show_finished: bool,
    /// History lines fetched above the screen for the preview.
    pub preview_history: usize,
    /// Restore grouping, filters and the selected agent from last time.
    pub remember: bool,
}

impl Default for Board {
    fn default() -> Self {
        Board { grouping: "project".into(), show_finished: true, preview_history: 200, remember: true }
    }
}

/// The workspace screen.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceCfg {
    /// The key that reaches Hiero's own controls: `ctrl+space` or `ctrl+<letter>`.
    pub prefix: String,
}

impl Default for WorkspaceCfg {
    fn default() -> Self {
        WorkspaceCfg { prefix: "ctrl+space".into() }
    }
}

/// The prefix key. Every other key goes to the program in the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix {
    Space,
    Ctrl(char),
}

impl Prefix {
    pub fn parse(s: &str) -> Result<Prefix, String> {
        let low = s.trim().to_lowercase();
        let bad = || format!("prefix must be ctrl+space or ctrl+<letter>, not `{s}`");
        let Some(key) = low.strip_prefix("ctrl+") else { return Err(bad()) };
        if key == "space" {
            return Ok(Prefix::Space);
        }
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            // These letters are Enter, Tab, Backspace and Ctrl+C on the wire.
            (Some(c @ ('c' | 'h' | 'i' | 'j' | 'm')), None) => {
                Err(format!("prefix ctrl+{c} can't be used: terminals send it for Enter, Tab, Backspace or interrupt"))
            }
            (Some(c @ 'a'..='z'), None) => Ok(Prefix::Ctrl(c)),
            _ => Err(bad()),
        }
    }

    /// Terminals disagree on ctrl+space: crossterm gives `Char(' ')` with
    /// CONTROL, or `Null` when the terminal sends a bare NUL byte.
    pub fn matches(&self, k: &crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT);
        match (self, k.code) {
            (Prefix::Space, KeyCode::Null) => true,
            (Prefix::Space, KeyCode::Char(' ')) => ctrl,
            (Prefix::Ctrl(c), KeyCode::Char(x)) => ctrl && x.to_ascii_lowercase() == *c,
            _ => false,
        }
    }

    /// What the program receives when the prefix is pressed twice.
    pub fn byte(&self) -> u8 {
        match self {
            Prefix::Space => 0,
            Prefix::Ctrl(c) => *c as u8 - b'a' + 1,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Prefix::Space => "ctrl+space".into(),
            Prefix::Ctrl(c) => format!("ctrl+{c}"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    pub bell: bool,
    pub desktop: bool,
    /// Which changes alert: blocked, error, done, exited, failed, pipeline.
    pub on: Vec<String>,
}

impl Default for Notify {
    fn default() -> Self {
        Notify {
            bell: true,
            desktop: true,
            on: ["blocked", "error", "done", "failed", "pipeline"].map(String::from).to_vec(),
        }
    }
}

impl Notify {
    pub fn wants(&self, what: &str) -> bool {
        self.on.iter().any(|w| w == what)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// The command line that starts it (default: the agent's name).
    pub command: Option<String>,
    /// How the task reaches it: "arg" (last argument, default), "send"
    /// (typed in once it's ready) or "none".
    pub task: Option<String>,
}

/// Keys that can be remapped, with their defaults.
pub const KEY_ACTIONS: &[(&str, char)] = &[
    ("new", 'c'),
    ("send", 's'),
    ("attach", 'a'),
    ("open", 'o'),
    ("interrupt", 'i'),
    ("kill", 'K'),
    ("remove", 'd'),
    ("keep", 'e'),
    ("rename", 'R'),
    ("next", 'n'),
    ("dismiss", 'x'),
    ("search", '/'),
    ("group", 'p'),
    ("finished", 'f'),
    ("review", 'v'),
    ("tasks", 'T'),
    ("pipelines", 'w'),
    ("broadcast", 'B'),
    ("templates", 't'),
    ("usage", 'u'),
    ("refresh", 'r'),
    ("help", '?'),
    ("quit", 'q'),
];

impl Config {
    /// Key typed → the default key it stands for. Remapped defaults stop
    /// working unless another action takes them, so nothing fires twice.
    pub fn key_map(&self) -> Result<BTreeMap<char, char>, String> {
        let mut map = BTreeMap::new();
        for (action, key) in &self.keys {
            let Some((_, default)) = KEY_ACTIONS.iter().find(|(a, _)| a == action) else {
                return Err(format!("[keys] unknown action `{action}`; known: {}", action_names()));
            };
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return Err(format!("[keys] {action} must be one character, got `{key}`"));
            };
            if map.insert(c, *default).is_some() {
                return Err(format!("[keys] `{c}` is used twice"));
            }
        }
        // A default key someone else took over, or that moved away, is disabled.
        for (action, default) in KEY_ACTIONS {
            if self.keys.contains_key(*action) && !map.contains_key(default) {
                map.insert(*default, '\0');
            }
        }
        Ok(map)
    }

    /// The workspace prefix key (`load` has checked it).
    pub fn prefix(&self) -> Prefix {
        Prefix::parse(&self.workspace.prefix).unwrap_or(Prefix::Space)
    }

    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.get(name)
    }
}

fn action_names() -> String {
    KEY_ACTIONS.iter().map(|(a, _)| *a).collect::<Vec<_>>().join(", ")
}

static CONFIG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();

/// The config this process runs with (defaults until `set` is called).
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

pub fn set(cfg: Config) {
    let _ = CONFIG.set(cfg);
}

/// The command line for an agent name, honouring `[agents.<name>] command`.
pub fn command_for(name: &str) -> String {
    get().agent(name).and_then(|a| a.command.clone()).unwrap_or_else(|| name.to_string())
}

/// How an agent gets its task: arg (default), send or none.
pub fn task_mode(name: &str) -> &'static str {
    match get().agent(name).and_then(|a| a.task.as_deref()) {
        Some("send") => "send",
        Some("none") => "none",
        _ => "arg",
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home("~/.config"));
    base.join("hiero").join("config.toml")
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home("~/.local/state"));
    base.join("hiero")
}

/// Load the config; a broken file is an error worth showing, not a crash.
pub fn load() -> Result<Config, String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    cfg.key_map()?;
    Prefix::parse(&cfg.workspace.prefix).map_err(|e| format!("[workspace] {e}"))?;
    if !matches!(cfg.board.grouping.as_str(), "project" | "tree" | "flat") {
        return Err(format!("[board] grouping must be project, tree or flat, not `{}`", cfg.board.grouping));
    }
    for (name, a) in &cfg.agents {
        if let Some(t) = &a.task
            && !matches!(t.as_str(), "arg" | "send" | "none")
        {
            return Err(format!("[agents.{name}] task must be arg, send or none, not `{t}`"));
        }
    }
    Ok(cfg)
}

/// A commented starting file.
pub const DEFAULT_TOML: &str = r#"# Hiero settings. Delete a line to get its default back.

[board]
grouping = "project"     # project, tree or flat (p cycles)
show_finished = true     # f toggles
preview_history = 200    # lines above the screen in the preview
remember = true          # restore grouping, filters and selection on start

[workspace]
prefix = "ctrl+space"    # reaches Hiero's controls; or ctrl+<letter> (not c, h, i, j, m)

[notify]
bell = true
desktop = true           # notify-send, or a Windows toast on WSL
# what alerts you: blocked, error, done, exited, failed, pipeline
on = ["blocked", "error", "done", "failed", "pipeline"]

# How to start an agent from the new-agent form and pipelines.
# task = "arg" (last argument, default) | "send" (typed in once started) | "none"
# [agents.claude]
# command = "claude --model opus"
# [agents.pi]
# task = "send"

# Remap board keys: action = "key". Actions: new send attach open interrupt kill
# remove keep rename next dismiss search group finished review tasks pipelines
# broadcast templates usage refresh help quit
[keys]
# kill = "X"
"#;

// ---- per-viewer UI state --------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub grouping: Option<String>,
    pub show_finished: Option<bool>,
    pub selected: Option<String>,
    pub query: String,
    /// The workspace open on the main screen (its id).
    pub workspace: Option<String>,
}

fn ui_state_path() -> PathBuf {
    state_dir().join("ui.json")
}

pub fn load_ui_state() -> UiState {
    std::fs::read_to_string(ui_state_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_ui_state(s: &UiState) {
    let path = ui_state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_parses_and_partial_files_work() {
        let c: Config = toml::from_str(DEFAULT_TOML).unwrap();
        assert_eq!(c.board.preview_history, 200);
        assert!(c.notify.wants("blocked") && !c.notify.wants("exited"));

        let c: Config = toml::from_str("[notify]\nbell = false\n[agents.pi]\ntask = \"send\"").unwrap();
        assert!(!c.notify.bell && c.notify.desktop, "unset fields keep defaults");
        assert_eq!(c.agent("pi").unwrap().task.as_deref(), Some("send"));
        assert!(toml::from_str::<Config>("[board]\ntypo = 1").is_err());
    }

    #[test]
    fn prefix_parsing() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert_eq!(Prefix::parse("ctrl+space"), Ok(Prefix::Space));
        assert_eq!(Prefix::parse(" Ctrl+B "), Ok(Prefix::Ctrl('b')));
        for bad in ["space", "alt+b", "ctrl+", "ctrl+bb", "ctrl+1", "ctrl+c", "ctrl+m"] {
            assert!(Prefix::parse(bad).is_err(), "{bad}");
        }
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(Prefix::Space.matches(&ctrl(' ')));
        assert!(Prefix::Space.matches(&KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)));
        assert!(!Prefix::Space.matches(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
        assert!(Prefix::Ctrl('b').matches(&ctrl('b')) && !Prefix::Ctrl('b').matches(&ctrl('a')));
        assert_eq!((Prefix::Space.byte(), Prefix::Ctrl('a').byte()), (0, 1));

        let c: Config = toml::from_str("[workspace]\nprefix = \"ctrl+a\"").unwrap();
        assert_eq!(c.prefix(), Prefix::Ctrl('a'));
        assert!(toml::from_str::<Config>("[workspace]\ntypo = 1").is_err());
        assert_eq!(toml::from_str::<Config>(DEFAULT_TOML).unwrap().prefix(), Prefix::Space);
    }

    #[test]
    fn key_remaps() {
        let mut c = Config::default();
        c.keys.insert("kill".into(), "X".into());
        let m = c.key_map().unwrap();
        assert_eq!(m.get(&'X'), Some(&'K'));
        assert_eq!(m.get(&'K'), Some(&'\0'), "old key disabled");

        // Swapping two keys works.
        let mut c = Config::default();
        c.keys.insert("kill".into(), "d".into());
        c.keys.insert("remove".into(), "K".into());
        let m = c.key_map().unwrap();
        assert_eq!((m[&'d'], m[&'K']), ('K', 'd'));

        let mut c = Config::default();
        c.keys.insert("explode".into(), "z".into());
        assert!(c.key_map().is_err());
    }
}
