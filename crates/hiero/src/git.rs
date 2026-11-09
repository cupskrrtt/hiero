//! Cheap git facts for a folder (repo root and branch), read straight from
//! `.git` so drawing never spawns `git`. Cached for a few seconds.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    /// The working tree's top folder.
    pub root: PathBuf,
    /// Branch name, or a short commit id when detached.
    pub branch: String,
}

impl GitInfo {
    /// The repo's folder name (for a linked worktree, still its own folder).
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }
}

const TTL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct GitCache {
    entries: HashMap<PathBuf, (Instant, Option<GitInfo>)>,
}

impl GitCache {
    pub fn lookup(&mut self, dir: &str) -> Option<GitInfo> {
        if dir.is_empty() {
            return None;
        }
        let key = PathBuf::from(dir);
        if let Some((at, info)) = self.entries.get(&key)
            && at.elapsed() < TTL
        {
            return info.clone();
        }
        let info = probe(&key);
        self.entries.insert(key, (Instant::now(), info.clone()));
        info
    }
}

/// Walk up from `dir` to the nearest `.git` and read its HEAD.
pub fn probe(dir: &Path) -> Option<GitInfo> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dotgit = d.join(".git");
        if let Ok(meta) = fs::metadata(&dotgit) {
            let gitdir = if meta.is_dir() {
                dotgit
            } else {
                // Linked worktree or submodule: "gitdir: <path>"
                let text = fs::read_to_string(&dotgit).ok()?;
                let p = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
                if p.is_absolute() { p } else { d.join(p) }
            };
            let head = fs::read_to_string(gitdir.join("HEAD")).ok()?;
            return Some(GitInfo {
                root: d.to_path_buf(),
                branch: branch_from_head(&head),
            });
        }
        cur = d.parent();
    }
    None
}

fn branch_from_head(head: &str) -> String {
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(r) => {
            let r = r.trim();
            r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
        }
        None => head.chars().take(7).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_head() {
        assert_eq!(branch_from_head("ref: refs/heads/feat/x\n"), "feat/x");
        assert_eq!(branch_from_head("0123456789abcdef\n"), "0123456");
    }

    #[test]
    fn finds_this_repo() {
        let here = env!("CARGO_MANIFEST_DIR");
        let info = probe(Path::new(here)).expect("hiero is a git repo");
        assert!(here.starts_with(info.root.to_str().unwrap()));
        assert!(!info.branch.is_empty());
    }
}
