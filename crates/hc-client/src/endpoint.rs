//! Where to find the core's socket, following herdr-core's own rules
//! (docs/security.md, "State and endpoints").

use std::env;
use std::path::PathBuf;

/// The session name: `HC_SESSION`, else `default`.
pub fn session() -> String {
    env::var("HC_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Resolve the client socket path.
///
/// Order: `HC_SOCKET`; `$XDG_RUNTIME_DIR/herdr-core/<session>.sock`;
/// `/run/user/<uid>/herdr-core/<session>.sock` when that runtime dir exists;
/// otherwise the session's state directory.
///
/// A custom `socket` set in `config.toml` is not read here; set `HC_SOCKET` instead.
pub fn socket_path() -> PathBuf {
    if let Some(p) = env::var_os("HC_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let file = format!("{}.sock", session());

    if let Some(rt) = env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty()) {
        return PathBuf::from(rt).join("herdr-core").join(file);
    }
    if let Some(uid) = current_uid() {
        let rt = PathBuf::from(format!("/run/user/{uid}"));
        if rt.is_dir() {
            return rt.join("herdr-core").join(file);
        }
    }
    state_dir().join(file)
}

/// `$XDG_STATE_HOME/herdr-core/<session>` or `~/.local/state/herdr-core/<session>`.
pub fn state_dir() -> PathBuf {
    let base = env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    base.join("herdr-core").join(session())
}

fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The real uid, read from /proc so we don't need libc.
fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}
