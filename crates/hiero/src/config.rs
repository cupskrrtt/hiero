//! `~/.config/hiero/config.toml`, plus per-viewer UI state in
//! `~/.local/state/hiero/ui.json` (on Windows `%APPDATA%\hiero` and `%LOCALAPPDATA%\hiero`). Everything has a default; a missing or
//! partly filled file is fine.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub workspace: WorkspaceCfg,
    pub notify: Notify,
    /// Sections of an older file that Hiero no longer reads (`load` fills
    /// this in). Not part of the file itself.
    #[serde(skip)]
    pub ignored: Vec<String>,
}

/// Sections from the days of the agent board. Old files may still have
/// them; they are skipped with a warning instead of failing the load.
pub const REMOVED_SECTIONS: &[&str] = &["board", "agents", "keys"];

/// The workspace screen.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceCfg {
    /// The key that reaches Hiero's own controls: `ctrl+space` or `ctrl+<letter>`.
    pub prefix: String,
    /// Start the terminals of every pane that has none (or lost it to a
    /// reboot) when a workspace opens. Off: they show "not running".
    pub restore: bool,
    /// Clicks, wheel and border drags in the workspace. Off: the terminal
    /// keeps the mouse (plain drag selects text).
    pub mouse: bool,
}

impl Default for WorkspaceCfg {
    fn default() -> Self {
        WorkspaceCfg { prefix: "ctrl+b".into(), restore: true, mouse: true }
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
    /// Which changes alert: blocked, error, done, exited, failed.
    pub on: Vec<String>,
}

impl Default for Notify {
    fn default() -> Self {
        Notify {
            bell: true,
            desktop: true,
            // The workspace raises "blocked" (needs you or error) and "done".
            on: ["blocked", "done"].map(String::from).to_vec(),
        }
    }
}

impl Notify {
    pub fn wants(&self, what: &str) -> bool {
        self.on.iter().any(|w| w == what)
    }
}

impl Config {
    /// The workspace prefix key (`load` has checked it).
    pub fn prefix(&self) -> Prefix {
        Prefix::parse(&self.workspace.prefix).unwrap_or(Prefix::Ctrl('b'))
    }
}

static CONFIG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();

/// The config this process runs with (defaults until `set` is called).
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

pub fn set(cfg: Config) {
    let _ = CONFIG.set(cfg);
}

pub fn config_path() -> PathBuf {
    paths::config_base().join("hiero").join("config.toml")
}

pub fn state_dir() -> PathBuf {
    paths::state_base().join("hiero")
}

/// Load the config; a broken file is an error worth showing, not a crash.
pub fn load() -> Result<Config, String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Parse a settings file. Sections of the removed board are dropped and
/// named in `ignored`; anything else unknown is still an error.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let ignored: Vec<String> = REMOVED_SECTIONS.iter().filter(|s| table.remove(**s).is_some()).map(|s| s.to_string()).collect();
    let mut cfg: Config = table.try_into().map_err(|e: toml::de::Error| e.to_string())?;
    Prefix::parse(&cfg.workspace.prefix).map_err(|e| format!("[workspace] {e}"))?;
    cfg.ignored = ignored;
    Ok(cfg)
}

impl Config {
    /// A warning about ignored sections of an old file, if there are any.
    pub fn ignored_warning(&self) -> Option<String> {
        if self.ignored.is_empty() {
            return None;
        }
        let names: Vec<String> = self.ignored.iter().map(|s| format!("[{s}]")).collect();
        Some(format!("{} in config.toml is from the removed agent board and is ignored", names.join(", ")))
    }
}

/// A commented starting file.
pub const DEFAULT_TOML: &str = r#"# Hiero settings. Delete a line to get its default back.

[workspace]
prefix = "ctrl+b"        # reaches Hiero's controls; ctrl+<letter> (not c, h, i, j, m) or ctrl+space
restore = true           # start missing terminals of a workspace when it opens
mouse = true             # clicks, wheel and border drags; hold Shift to select text instead

[notify]
bell = true
desktop = true           # notify-send, or a Windows toast (Windows and WSL)
# what alerts you: blocked (a workspace needs you, or a program errored), done
on = ["blocked", "done"]
"#;

// ---- per-viewer UI state --------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// The workspace open on the main screen (its id).
    pub workspace: Option<String>,
    /// Sidebar width in columns (None: the default).
    pub sidebar_width: Option<u16>,
    pub sidebar_hidden: bool,
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
        assert!(c.notify.wants("blocked") && !c.notify.wants("exited"));

        let c: Config = toml::from_str("[notify]\nbell = false").unwrap();
        assert!(!c.notify.bell && c.notify.desktop, "unset fields keep defaults");
        assert!(toml::from_str::<Config>("[notify]\ntypo = 1").is_err());
    }

    #[test]
    fn removed_sections_are_ignored_with_a_warning() {
        let old = "[board]\ngrouping = \"tree\"\n[agents.pi]\ntask = \"send\"\n[keys]\nkill = \"X\"\n[notify]\nbell = false\n";
        let c = parse(old).unwrap();
        assert!(!c.notify.bell, "the rest still applies");
        assert_eq!(c.ignored, ["board", "agents", "keys"]);
        assert!(c.ignored_warning().unwrap().contains("[board], [agents], [keys]"));
        assert!(parse(DEFAULT_TOML).unwrap().ignored_warning().is_none());
        assert!(parse("[typo]\nx = 1").is_err(), "other unknown sections still fail");
        assert!(parse("[workspace]\nprefix = \"ctrl+c\"").is_err());
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
        assert_eq!(toml::from_str::<Config>(DEFAULT_TOML).unwrap().prefix(), Prefix::Ctrl('b'));
        assert_eq!(Config::default().prefix(), Prefix::Ctrl('b'));
        assert!(Config::default().workspace.restore);
        assert!(toml::from_str::<Config>(DEFAULT_TOML).unwrap().workspace.restore);
        let c: Config = toml::from_str("[workspace]\nrestore = false").unwrap();
        assert!(!c.workspace.restore);
        assert!(Config::default().workspace.mouse);
        assert!(toml::from_str::<Config>(DEFAULT_TOML).unwrap().workspace.mouse);
        let c: Config = toml::from_str("[workspace]\nmouse = false").unwrap();
        assert!(!c.workspace.mouse);
    }
}
