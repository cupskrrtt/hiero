//! Status bridge: agent hooks → OSC 7501 status reports.
//!
//! herdr-core only knows a status that the program reports itself. Claude Code
//! doesn't, but it runs hooks on every step. `hiero hook claude <Event>` turns
//! each hook into a report written to the agent's terminal, where hc reads it.
//! `hiero integrate claude` installs those hooks in Claude Code's settings.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Marks hook commands Hiero owns, so install and uninstall touch only those.
const MARKER: &str = "hook claude";

/// The Claude Code hook events Hiero listens to.
pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Notification",
    "Stop",
    "PreCompact",
];

/// One status report (OSC 7501 root record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub state: &'static str,
    pub kind: Option<&'static str>,
    pub msg: String,
}

impl Report {
    fn new(state: &'static str) -> Self {
        Report { state, kind: None, msg: String::new() }
    }
    fn kind(mut self, k: &'static str) -> Self {
        self.kind = Some(k);
        self
    }
    fn msg(mut self, m: impl Into<String>) -> Self {
        self.msg = m.into();
        self
    }

    /// `ESC ] 7501 ; state=…:kind=…:app=…:msg=<base64> ESC \`
    pub fn osc(&self, app: &str) -> String {
        let mut fields = vec![format!("state={}", self.state)];
        if let Some(k) = self.kind {
            fields.push(format!("kind={k}"));
        }
        fields.push(format!("app={app}"));
        if !self.msg.is_empty() {
            fields.push(format!("msg={}", base64(self.msg.as_bytes())));
        }
        format!("\x1b]7501;{}\x1b\\", fields.join(":"))
    }
}

/// Map one Claude Code hook call to a status, or `None` to leave it as is.
pub fn claude_report(event: &str, input: &Value) -> Option<Report> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match event {
        "SessionStart" => Some(Report::new("idle")),
        "UserPromptSubmit" => Some(Report::new("working")),
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            Some(Report::new("working").msg(tool_summary(input)))
        }
        "PreCompact" => Some(Report::new("working").msg("compacting the conversation")),
        "PermissionRequest" => {
            let what = tool_summary(input);
            let msg = if what.is_empty() { "permission needed".to_string() } else { format!("allow {what}?") };
            Some(Report::new("blocked").kind("permission").msg(msg))
        }
        "Notification" => {
            let message = s("message");
            match s("notification_type") {
                "permission_prompt" => Some(Report::new("blocked").kind("permission").msg(message)),
                "elicitation_dialog" => Some(Report::new("blocked").kind("question").msg(message)),
                // Waiting for the next prompt after Stop: already done.
                "idle_prompt" | "auth_success" => None,
                // Older versions send no type; read the message.
                _ if message.to_lowercase().contains("permission") => {
                    Some(Report::new("blocked").kind("permission").msg(message))
                }
                _ => None,
            }
        }
        "Stop" => Some(Report::new("done")),
        _ => None,
    }
}

/// "Bash: cargo test", "Edit: src/main.rs", …
fn tool_summary(input: &Value) -> String {
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if tool.is_empty() {
        return String::new();
    }
    let ti = input.get("tool_input");
    let detail = ["command", "file_path", "path", "pattern", "url", "description", "prompt"]
        .iter()
        .find_map(|k| ti.and_then(|t| t.get(*k)).and_then(Value::as_str))
        .unwrap_or("");
    let detail: String = detail.lines().next().unwrap_or("").chars().take(120).collect();
    if detail.is_empty() { tool.to_string() } else { format!("{tool}: {detail}") }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

// ---- the hook itself ----------------------------------------------------------

/// The `app` Claude Code uses when it reports status itself (2.1.319+).
const CLAUDE_NATIVE_APP: &str = "claude-code";

/// Run as a hook. Prints nothing and always succeeds, so it can never
/// disturb or block the agent.
///
/// - `claude <Event>`: Claude Code hook; the event JSON comes on stdin.
/// - `codex [json]`: Codex `notify`; the JSON comes as the last argument.
pub async fn run_hook(agent: &str, event: &str, arg: Option<&str>) {
    let mut raw = String::new();
    if agent == "claude" {
        let _ = std::io::stdin().read_to_string(&mut raw);
    }
    // Only inside hc terminals; elsewhere nobody is listening.
    let Ok(terminal) = std::env::var("HC_TERMINAL_ID") else { return };
    if std::env::var_os("HIERO_HOOK_OFF").is_some() {
        return;
    }
    let report = match agent {
        "claude" => {
            let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            // Subagents run inside the same terminal; their stops aren't the agent's.
            if input.get("agent_id").is_some_and(|v| !v.is_null()) && event == "Stop" {
                return;
            }
            // Newer Claude Code reports by itself; never fight it.
            if reports_natively(&terminal, CLAUDE_NATIVE_APP).await {
                return;
            }
            claude_report(event, &input)
        }
        "codex" => {
            // `notify` passes one JSON argument; clap gave it to us as `event`
            // when nothing else followed.
            let json = arg.unwrap_or(event);
            codex_report(&serde_json::from_str(json).unwrap_or(Value::Null))
        }
        _ => None,
    };
    if let Some(report) = report {
        deliver(&terminal, agent, &report).await;
    }
}

/// Write a report to this terminal, or fall back to labels.
pub async fn deliver(terminal: &str, app: &str, report: &Report) {
    if write_tty(&report.osc(app)).is_err() {
        // No controlling terminal: fall back to labels Hiero reads.
        let _ = label_fallback(terminal, report).await;
    }
}

/// Whether the agent in `terminal` already reports its own status as `app`.
async fn reports_natively(terminal: &str, app: &str) -> bool {
    let check = async {
        let hc = hc_client::Client::connect().await.ok()?;
        let t = hc.get(terminal).await.ok()?;
        Some(t.status.is_some_and(|s| s.app == app) || t.records.values().any(|s| s.app == app))
    };
    tokio::time::timeout(std::time::Duration::from_millis(500), check)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Codex `notify`: only turn completion is reported, so that's `done`.
pub fn codex_report(input: &Value) -> Option<Report> {
    match input.get("type").and_then(Value::as_str) {
        Some("agent-turn-complete") => {
            let last = input.get("last-assistant-message").and_then(Value::as_str).unwrap_or("");
            let msg: String = last.lines().next().unwrap_or("").chars().take(160).collect();
            Some(Report::new("done").msg(msg))
        }
        _ => None,
    }
}

/// Parse a state word for `hiero report`.
pub fn parse_state(word: &str) -> Option<&'static str> {
    ["idle", "working", "done", "blocked", "error", "clear"].into_iter().find(|s| *s == word)
}

pub fn parse_kind(word: &str) -> Option<&'static str> {
    ["permission", "question", "auth"].into_iter().find(|s| *s == word)
}

/// `hiero report`: report a status for the terminal this runs in.
pub async fn report_cmd(state: &str, kind: Option<&str>, msg: Option<&str>, app: &str) -> Result<()> {
    let state = parse_state(state).context("state must be idle, working, done, blocked, error or clear")?;
    let kind = match kind {
        Some(k) => Some(parse_kind(k).context("kind must be permission, question or auth")?),
        None => None,
    };
    let terminal = std::env::var("HC_TERMINAL_ID").context("not inside an hc terminal (HC_TERMINAL_ID is unset)")?;
    let report = Report { state, kind, msg: msg.unwrap_or("").to_string() };
    deliver(&terminal, app, &report).await;
    Ok(())
}

/// The Claude Code version, from `claude --version`.
pub fn claude_version() -> Option<(u32, u32, u32)> {
    let out = std::process::Command::new("claude").arg("--version").output().ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text.split_whitespace().next()?;
    let mut it = word.split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// The oldest Claude Code version seen reporting status itself.
pub const CLAUDE_NATIVE_SINCE: (u32, u32, u32) = (2, 1, 319);

fn write_tty(seq: &str) -> std::io::Result<()> {
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(seq.as_bytes())?;
    tty.flush()
}

pub const LABEL_STATUS: &str = "hiero.status";
pub const LABEL_STATUS_KIND: &str = "hiero.status_kind";
pub const LABEL_STATUS_MSG: &str = "hiero.status_msg";
pub const LABEL_STATUS_MS: &str = "hiero.status_ms";

async fn label_fallback(terminal: &str, r: &Report) -> Result<()> {
    let hc = tokio::time::timeout(std::time::Duration::from_secs(2), hc_client::Client::connect()).await??;
    let labels = hc_client::Labels::from([
        (LABEL_STATUS.to_string(), r.state.to_string()),
        (LABEL_STATUS_KIND.to_string(), r.kind.unwrap_or("").to_string()),
        (LABEL_STATUS_MSG.to_string(), r.msg.chars().take(200).collect()),
        (LABEL_STATUS_MS.to_string(), crate::app::now_ms().to_string()),
    ]);
    tokio::time::timeout(std::time::Duration::from_secs(2), hc.label(terminal, &labels)).await??;
    Ok(())
}

// ---- installing ---------------------------------------------------------------

pub fn claude_settings_path() -> PathBuf {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::forms::expand_home("~/.claude"));
    base.join("settings.json")
}

fn hook_command(exe: &Path, event: &str) -> String {
    let exe = shell_words::quote(&exe.to_string_lossy()).into_owned();
    format!("{exe} {MARKER} {event}")
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(&format!(" {MARKER} ")))
}

/// Remove Hiero's hooks from a settings value; returns how many went.
pub fn remove_hooks(settings: &mut Value) -> usize {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        for group in groups.iter_mut() {
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = list.len();
                list.retain(|h| !is_ours(h));
                removed += before - list.len();
            }
        }
        // Drop groups we emptied.
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|l| !l.is_empty()));
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    if hooks.is_empty()
        && let Some(obj) = settings.as_object_mut()
    {
        obj.remove("hooks");
    }
    removed
}

/// Add Hiero's hooks (replacing any older ones), keeping everything else.
pub fn add_hooks(settings: &mut Value, exe: &Path) -> Result<()> {
    remove_hooks(settings);
    if !settings.is_object() {
        bail!("settings file is not a JSON object");
    }
    let obj = settings.as_object_mut().unwrap();
    let hooks = obj.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().context("\"hooks\" is not an object")?;
    for event in CLAUDE_EVENTS {
        let mut group = json!({
            "hooks": [{ "type": "command", "command": hook_command(exe, event), "timeout": 5 }]
        });
        if matches!(*event, "PreToolUse" | "PostToolUse" | "PermissionRequest") {
            group.as_object_mut().unwrap().insert("matcher".into(), json!("*"));
        }
        let groups = hooks.entry(*event).or_insert_with(|| json!([]));
        groups.as_array_mut().context("hook event is not a list")?.push(group);
    }
    Ok(())
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("can't read {}", path.display())),
    }
}

fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if path.exists() {
        let backup = path.with_extension("json.hiero-backup");
        std::fs::copy(path, &backup).with_context(|| format!("can't back up to {}", backup.display()))?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.hiero-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn install_claude(settings_path: &Path, exe: &Path) -> Result<()> {
    let mut settings = read_settings(settings_path)?;
    add_hooks(&mut settings, exe)?;
    write_settings(settings_path, &settings)
}

pub fn uninstall_claude(settings_path: &Path) -> Result<usize> {
    let mut settings = read_settings(settings_path)?;
    let n = remove_hooks(&mut settings);
    if n > 0 {
        write_settings(settings_path, &settings)?;
    }
    Ok(n)
}

/// Which events currently run Hiero's hook, and with which command.
pub fn claude_status(settings_path: &Path) -> Result<Vec<(String, String)>> {
    let settings = read_settings(settings_path)?;
    let mut out = Vec::new();
    if let Some(hooks) = settings.get("hooks").and_then(Value::as_object) {
        for (event, groups) in hooks {
            for g in groups.as_array().into_iter().flatten() {
                for h in g.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                    if is_ours(h) {
                        out.push((event.clone(), h["command"].as_str().unwrap_or("").to_string()));
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_and_versions() {
        let r = codex_report(&json!({"type":"agent-turn-complete","last-assistant-message":"All tests pass.\nmore"})).unwrap();
        assert_eq!((r.state, r.msg.as_str()), ("done", "All tests pass."));
        assert!(codex_report(&json!({"type":"other"})).is_none());
        assert_eq!(parse_version("2.1.319 (Claude Code)\n"), Some((2, 1, 319)));
        assert!(parse_version("garbage").is_none());
        assert_eq!(parse_state("blocked"), Some("blocked"));
        assert!(parse_state("nope").is_none());
    }

    #[test]
    fn osc_matches_hc_format() {
        let r = Report::new("blocked").kind("permission").msg("ok");
        assert_eq!(r.osc("claude"), "\x1b]7501;state=blocked:kind=permission:app=claude:msg=b2s=\x1b\\");
        assert_eq!(base64(b"Allow"), "QWxsb3c=");
    }

    #[test]
    fn maps_claude_events() {
        let r = claude_report(
            "PermissionRequest",
            &json!({"tool_name":"Bash","tool_input":{"command":"rm -rf target\necho"}}),
        )
        .unwrap();
        assert_eq!((r.state, r.kind), ("blocked", Some("permission")));
        assert_eq!(r.msg, "allow Bash: rm -rf target?");

        assert_eq!(claude_report("UserPromptSubmit", &json!({})).unwrap().state, "working");
        assert_eq!(claude_report("Stop", &json!({})).unwrap().state, "done");
        assert_eq!(
            claude_report("PostToolUse", &json!({"tool_name":"Edit","tool_input":{"file_path":"a.rs"}})).unwrap().msg,
            "Edit: a.rs"
        );
        let n = claude_report(
            "Notification",
            &json!({"notification_type":"elicitation_dialog","message":"Pick one"}),
        )
        .unwrap();
        assert_eq!((n.state, n.kind), ("blocked", Some("question")));
        assert!(claude_report("Notification", &json!({"notification_type":"idle_prompt"})).is_none());
        assert_eq!(
            claude_report("Notification", &json!({"message":"Claude needs your permission to use Bash"}))
                .unwrap()
                .state,
            "blocked"
        );
        assert!(claude_report("SubagentStop", &json!({})).is_none());
    }

    #[test]
    fn install_keeps_other_hooks_and_is_idempotent() {
        let herdr = json!({"type":"command","command":"bash '/x/herdr-agent-state.sh' session","timeout":10});
        let mut s = json!({
            "theme": "dark",
            "hooks": {"SessionStart": [{"matcher":"^(startup)$","hooks":[herdr.clone()]}]}
        });
        let exe = Path::new("/opt/my tools/hiero");
        add_hooks(&mut s, exe).unwrap();
        add_hooks(&mut s, exe).unwrap(); // twice: no duplicates

        let start = s["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2);
        assert_eq!(start[0]["hooks"][0], herdr);
        assert_eq!(
            start[1]["hooks"][0]["command"],
            "'/opt/my tools/hiero' hook claude SessionStart"
        );
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "*");
        assert_eq!(s["hooks"].as_object().unwrap().len(), CLAUDE_EVENTS.len());

        assert_eq!(remove_hooks(&mut s), CLAUDE_EVENTS.len());
        assert_eq!(s, json!({"theme":"dark","hooks":{"SessionStart":[{"matcher":"^(startup)$","hooks":[herdr]}]}}));

        let mut empty = json!({"theme":"dark"});
        add_hooks(&mut empty, exe).unwrap();
        remove_hooks(&mut empty);
        assert_eq!(empty, json!({"theme":"dark"}));
    }
}
