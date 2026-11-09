//! Where to find the core's endpoint (a Unix socket, or a named pipe on Windows),
//! following herdr-core's own rules (docs/security.md, "State and endpoints").

use std::env;
use std::path::PathBuf;

/// The session name: `HC_SESSION`, else `default`.
pub fn session() -> String {
    env::var("HC_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Resolve the client endpoint: a socket path on Unix, a pipe name on Windows.
///
/// `HC_SOCKET` always wins. On Unix then: `$XDG_RUNTIME_DIR/herdr-core/<session>.sock`;
/// `/run/user/<uid>/herdr-core/<session>.sock` when that runtime dir exists;
/// otherwise the session's state directory. On Windows: the pipe
/// `\\.\pipe\herdr-core-<user>-<session>.sock`.
///
/// A custom `socket` set in `config.toml` is not read here; set `HC_SOCKET` instead.
pub fn socket_path() -> PathBuf {
    if let Some(p) = env::var_os("HC_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    default_endpoint()
}

#[cfg(windows)]
fn default_endpoint() -> PathBuf {
    let user = env::var("USERNAME").unwrap_or_default();
    PathBuf::from(pipe_name(&user, &session()))
}

#[cfg(unix)]
fn default_endpoint() -> PathBuf {
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

/// The Windows pipe name; herdr-core keeps only ASCII letters and digits of the user name.
#[cfg_attr(unix, allow(dead_code))]
fn pipe_name(user: &str, session: &str) -> String {
    let user: String = user.chars().filter(char::is_ascii_alphanumeric).collect();
    format!(r"\\.\pipe\herdr-core-{user}-{session}.sock")
}

/// The session's state directory: `$XDG_STATE_HOME/herdr-core/<session>` or
/// `~/.local/state/herdr-core/<session>` (Windows: `%LOCALAPPDATA%\herdr-core\<session>`).
pub fn state_dir() -> PathBuf {
    state_base().join("herdr-core").join(session())
}

#[cfg(unix)]
fn state_base() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}

#[cfg(windows)]
fn state_base() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"))
}

#[cfg(unix)]
fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(windows)]
fn home() -> PathBuf {
    env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\"))
}

/// The real uid, read from /proc so we don't need libc.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_keeps_alphanumerics_only() {
        assert_eq!(
            pipe_name("luthfi.khan", "default"),
            r"\\.\pipe\herdr-core-luthfikhan-default.sock"
        );
        assert_eq!(pipe_name("A b_9", "wintest"), r"\\.\pipe\herdr-core-Ab9-wintest.sock");
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_ends_with_session_socket() {
        // HC_SOCKET is not set by the test harness; the default must be <dir>/herdr-core*/<session>.sock.
        if env::var_os("HC_SOCKET").is_none() {
            assert!(socket_path().to_string_lossy().ends_with(".sock"));
        }
    }
}
