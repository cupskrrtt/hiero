//! Where Hiero keeps things, and how it spells folders, on Unix and Windows.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The home directory (`~`): `HOME`, or `USERPROFILE` on Windows.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `~` and `~/rest` (also `~\rest` on Windows) expanded; anything else as is.
pub fn expand_home(p: &str) -> PathBuf {
    let sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    match (p.strip_prefix('~'), home_dir()) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(sep) => {
            home.join(rest.trim_start_matches(sep))
        }
        _ => PathBuf::from(p),
    }
}

/// An explicit XDG variable always wins (like herdr-core); otherwise on Windows
/// the per-user app folder, otherwise `~/<unix_default>`.
fn base_dir(xdg: Option<OsString>, windows_dir: Option<OsString>, unix_default: &str) -> PathBuf {
    let set = |v: Option<OsString>| v.filter(|p| !p.is_empty()).map(PathBuf::from);
    if let Some(p) = set(xdg) {
        return p;
    }
    if cfg!(windows)
        && let Some(p) = set(windows_dir)
    {
        return p;
    }
    expand_home(&format!("~/{unix_default}"))
}

/// Config folder: `$XDG_CONFIG_HOME`, `%APPDATA%`, or `~/.config`.
pub fn config_base() -> PathBuf {
    base_dir(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("APPDATA"), ".config")
}

/// State and logs folder: `$XDG_STATE_HOME`, `%LOCALAPPDATA%`, or `~/.local/state`.
pub fn state_base() -> PathBuf {
    base_dir(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("LOCALAPPDATA"), ".local/state")
}

/// Drop the `\\?\` prefix Windows `canonicalize` adds, so folders read as `C:\x`.
/// Other verbatim forms (devices, odd names) are left alone.
pub fn strip_verbatim(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = p.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'\\') {
            return rest.to_string();
        }
    }
    p.to_string()
}

/// The canonical absolute form of `p` as text, with no verbatim prefix.
pub fn canonical(p: &Path) -> std::io::Result<String> {
    Ok(strip_verbatim(&p.canonicalize()?.to_string_lossy()))
}

/// The program's name in a command line's first word: the part after the last separator.
pub fn program_name(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

/// Arguments as one shell line (a pane stores its command that way): words that
/// need it are quoted, with single quotes on Unix and double quotes on Windows.
pub fn shell_join<S: AsRef<str>>(args: &[S]) -> String {
    let plain = |a: &str| !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c) || (cfg!(windows) && c == '\\'));
    let quote = |a: &str| {
        if plain(a) {
            a.to_string()
        } else if cfg!(windows) {
            format!("\"{}\"", a.replace('"', "\\\""))
        } else {
            format!("'{}'", a.replace('\'', "'\\''"))
        }
    };
    args.iter().map(|a| quote(a.as_ref())).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes_only_what_needs_it() {
        assert_eq!(shell_join(&["nvim", "-c", "a/b.rs"]), "nvim -c a/b.rs");
        if cfg!(windows) {
            assert_eq!(shell_join(&["echo", "a b", ""]), r#"echo "a b" """#);
        } else {
            assert_eq!(shell_join(&["echo", "a b", "", "it's", "$HOME;ls"]), r#"echo 'a b' '' 'it'\''s' '$HOME;ls'"#);
        }
    }

    #[test]
    fn verbatim_prefix_is_stripped_only_when_safe() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\me\my proj"), r"C:\Users\me\my proj");
        assert_eq!(strip_verbatim(r"\\?\c:"), "c:");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(strip_verbatim(r"\\?\Volume{1234}\x"), r"\\?\Volume{1234}\x");
        assert_eq!(strip_verbatim(r"\\?\C:x"), r"\\?\C:x");
        assert_eq!(strip_verbatim("/home/me"), "/home/me");
        assert_eq!(strip_verbatim(r"C:\plain"), r"C:\plain");
    }

    #[test]
    fn explicit_xdg_wins_then_windows_dir() {
        let s = |v: &str| Some(OsString::from(v));
        assert_eq!(base_dir(s("/x"), s("/w"), ".config"), PathBuf::from("/x"));
        if cfg!(windows) {
            assert_eq!(base_dir(None, s("/w"), ".config"), PathBuf::from("/w"));
            assert_eq!(base_dir(s(""), s("/w"), ".config"), PathBuf::from("/w"));
        } else {
            assert!(base_dir(None, s("/w"), ".config").ends_with(".config"));
        }
    }

    #[test]
    fn program_name_handles_both_separators() {
        assert_eq!(program_name("/usr/bin/nvim"), "nvim");
        assert_eq!(program_name(r"C:\tools\nvim.exe"), "nvim.exe");
        assert_eq!(program_name("nvim"), "nvim");
    }

    #[cfg(windows)]
    #[test]
    fn canonical_has_no_verbatim_prefix() {
        let c = canonical(&std::env::temp_dir()).unwrap();
        assert!(!c.starts_with(r"\\?\"), "{c}");
        assert!(c.as_bytes()[1] == b':', "{c}");
    }

    #[cfg(windows)]
    #[test]
    fn expand_home_takes_backslash() {
        let home = home_dir().unwrap();
        assert_eq!(expand_home(r"~\a\b"), home.join(r"a\b"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~x"), PathBuf::from("~x"));
    }
}
