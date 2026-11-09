//! Git worktrees for tasks, through the `git` CLI (so the person's own git
//! config, hooks and credentials apply). Everything here blocks; call it
//! from `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::process::Command;

pub type GitResult<T> = Result<T, String>;

/// Run git in `dir`; stdout on success, stderr (or stdout) as the error.
pub fn git(dir: &Path, args: &[&str]) -> GitResult<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("can't run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { err };
        let err: Vec<&str> = err.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        Err(format!("git {}: {}", args.first().unwrap_or(&""), err.join(" · ")))
    }
}

/// The top folder of the main working tree that `dir` belongs to.
pub fn repo_root(dir: &Path) -> GitResult<PathBuf> {
    // --git-common-dir points at the main repo's .git even from a linked worktree.
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = PathBuf::from(common.trim());
    common
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "can't find the repository root".into())
}

pub fn current_branch(dir: &Path) -> GitResult<String> {
    Ok(git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string())
}

pub fn head_commit(dir: &Path) -> GitResult<String> {
    Ok(git(dir, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// A branch- and folder-safe name from free text: "Fix the login bug!" → "fix-the-login-bug".
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
    {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word.to_ascii_lowercase());
        if out.len() >= 40 {
            break;
        }
    }
    out.truncate(40);
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "task".into() } else { out }
}

/// Where task worktrees for `repo` live: a sibling folder `<repo>.hiero/`.
pub fn worktrees_home(repo: &Path) -> PathBuf {
    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    repo.parent().unwrap_or(repo).join(format!("{name}.hiero"))
}

#[derive(Debug, Clone)]
pub struct Created {
    pub repo: PathBuf,
    pub base_branch: String,
    pub base_commit: String,
    pub branch: String,
    pub slug: String,
    pub path: PathBuf,
}

/// Make a worktree on a new branch `hiero/<slug>` from the repo's current HEAD.
/// Picks `slug-2`, `slug-3`, … when the branch or folder is taken.
pub fn create(dir: &Path, wanted_slug: &str) -> GitResult<Created> {
    let repo = repo_root(dir)?;
    let base_branch = current_branch(&repo)?;
    let base_commit = head_commit(&repo)?;
    let home = worktrees_home(&repo);
    let mut slug = wanted_slug.to_string();
    for n in 2.. {
        let branch = format!("hiero/{slug}");
        let taken = git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
        if !taken && !home.join(&slug).exists() {
            break;
        }
        slug = format!("{wanted_slug}-{n}");
    }
    let branch = format!("hiero/{slug}");
    let path = home.join(&slug);
    std::fs::create_dir_all(&home).map_err(|e| format!("can't make {}: {e}", home.display()))?;
    git(&repo, &["worktree", "add", "-b", &branch, &path.to_string_lossy(), &base_commit])?;
    Ok(Created { repo, base_branch: if base_branch == "HEAD" { base_commit.clone() } else { base_branch }, base_commit, branch, slug, path })
}

/// Uncommitted changes (including new files) in a worktree.
pub fn is_dirty(dir: &Path) -> GitResult<bool> {
    Ok(!git(dir, &["status", "--porcelain"])?.trim().is_empty())
}

/// Commit everything in the worktree; false when there was nothing to commit.
pub fn commit_all(dir: &Path, message: &str) -> GitResult<bool> {
    if !is_dirty(dir)? {
        return Ok(false);
    }
    git(dir, &["add", "-A"])?;
    git(dir, &["commit", "--no-verify", "-m", message])?;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub added: Option<u32>,
    pub removed: Option<u32>,
    /// M, A (added), D, R, ? (new, not yet in git)
    pub status: char,
}

#[derive(Debug, Clone, Default)]
pub struct Review {
    pub files: Vec<FileChange>,
    /// Diff text per file path.
    pub diffs: Vec<(String, String)>,
    pub commits_ahead: u32,
    pub dirty: bool,
    /// The commit the diff is against.
    pub base: String,
}

impl Review {
    pub fn diff_for(&self, path: &str) -> &str {
        self.diffs.iter().find(|(p, _)| p == path).map(|(_, d)| d.as_str()).unwrap_or("")
    }
}

/// Everything the task changed relative to where its branch forked from
/// `base_branch`: commits plus uncommitted edits plus new files.
pub fn review(worktree: &Path, base_branch: &str) -> GitResult<Review> {
    if !worktree.is_dir() {
        return Err(format!("the worktree {} is gone", worktree.display()));
    }
    let base = git(worktree, &["merge-base", base_branch, "HEAD"])
        .map(|s| s.trim().to_string())
        .or_else(|_| head_commit(worktree))?;
    let commits_ahead = git(worktree, &["rev-list", "--count", &format!("{base}..HEAD")])?
        .trim()
        .parse()
        .unwrap_or(0);

    let mut files = Vec::new();
    let status_out = git(worktree, &["diff", "--name-status", "-M", &base])?;
    let numstat = git(worktree, &["diff", "--numstat", "-M", &base])?;
    for (line, nums) in status_out.lines().zip(numstat.lines()) {
        let mut parts = line.split('\t');
        let status = parts.next().and_then(|s| s.chars().next()).unwrap_or('M');
        let path = parts.next_back().unwrap_or("").to_string();
        let mut n = nums.split('\t');
        files.push(FileChange {
            path,
            added: n.next().and_then(|v| v.parse().ok()),
            removed: n.next().and_then(|v| v.parse().ok()),
            status,
        });
    }
    let patch = git(worktree, &["diff", "-M", "--no-color", &base])?;
    let mut diffs = split_patch(&patch);

    // New files git doesn't track yet.
    let untracked = git(worktree, &["ls-files", "--others", "--exclude-standard"])?;
    for path in untracked.lines().filter(|l| !l.is_empty()) {
        let full = worktree.join(path);
        let text = match std::fs::read(&full) {
            Ok(bytes) if bytes.contains(&0) => "(binary file)".to_string(),
            Ok(bytes) => {
                let s = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = s.lines().collect();
                let mut out = format!("new file {path}\n@@ -0,0 +1,{} @@\n", lines.len());
                for l in lines.iter().take(500) {
                    out.push('+');
                    out.push_str(l);
                    out.push('\n');
                }
                if lines.len() > 500 {
                    out.push_str(&format!("… {} more lines\n", lines.len() - 500));
                }
                out
            }
            Err(e) => format!("(can't read: {e})"),
        };
        let added = text.lines().filter(|l| l.starts_with('+')).count() as u32;
        files.push(FileChange { path: path.to_string(), added: Some(added), removed: Some(0), status: '?' });
        diffs.push((path.to_string(), text));
    }

    Ok(Review { files, diffs, commits_ahead, dirty: is_dirty(worktree)?, base })
}

/// Split a unified diff into per-file chunks, keyed by the new path.
fn split_patch(patch: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map(|(_, b)| b.to_string()).unwrap_or_else(|| rest.to_string());
            out.push((path, String::new()));
        }
        if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

/// Merge the task branch into its base branch in the main working tree.
/// Refuses when the main tree is on another branch or has uncommitted
/// changes; aborts cleanly on conflicts.
pub fn merge(repo: &Path, base_branch: &str, branch: &str, message: &str) -> GitResult<()> {
    let on = current_branch(repo)?;
    if on != base_branch {
        return Err(format!("{} is on `{on}`, not `{base_branch}` — switch it first", repo.display()));
    }
    if !git(repo, &["status", "--porcelain", "--untracked-files=no"])?.trim().is_empty() {
        return Err(format!("{} has uncommitted changes — commit or stash them first", repo.display()));
    }
    if let Err(e) = git(repo, &["merge", "--no-ff", "--no-edit", "-m", message, branch]) {
        let _ = git(repo, &["merge", "--abort"]);
        return Err(format!("merge failed and was undone: {e}"));
    }
    Ok(())
}

/// Remove the worktree folder and delete its branch.
pub fn remove(repo: &Path, path: &Path, branch: &str) -> GitResult<()> {
    if path.exists() {
        git(repo, &["worktree", "remove", "--force", &path.to_string_lossy()])?;
    }
    let _ = git(repo, &["worktree", "prune"]);
    if git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
        git(repo, &["branch", "-D", branch])?;
    }
    // Leave no empty `<repo>.hiero/` behind.
    if let Some(home) = path.parent() {
        let _ = std::fs::remove_dir(home);
    }
    Ok(())
}

/// Push the branch and open a pull request with `gh`; returns its URL.
pub fn open_pr(worktree: &Path, repo: &Path, base_branch: &str, branch: &str, title: &str, body: &str) -> GitResult<String> {
    git(worktree, &["push", "-u", "origin", branch])?;
    let out = Command::new("gh")
        .current_dir(repo)
        .args(["pr", "create", "--head", branch, "--base", base_branch, "--title", title, "--body", body])
        .output()
        .map_err(|e| format!("can't run gh (is it installed?): {e}"))?;
    if !out.status.success() {
        return Err(format!("gh pr create: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().rev().find(|l| l.starts_with("http")).unwrap_or(text.trim()).to_string())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway repo with one commit on `main`.
    pub fn temp_repo() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "hiero-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            git(&repo, &args).unwrap();
        }
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-qm", "init"]).unwrap();
        repo
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(slugify("¿?"), "task");
        assert!(slugify(&"word ".repeat(30)).len() <= 40);
    }

    #[test]
    fn create_review_merge_remove() {
        let repo = temp_repo();
        let c = create(&repo, "add-b").unwrap();
        assert_eq!(c.branch, "hiero/add-b");
        assert_eq!(c.base_branch, "main");
        assert!(c.path.ends_with("proj.hiero/add-b"));
        // A second task with the same name gets a fresh slug.
        let c2 = create(&repo, "add-b").unwrap();
        assert_eq!(c2.slug, "add-b-2");
        remove(&repo, &c2.path, &c2.branch).unwrap();

        // Work in the worktree: edit, add a new file.
        std::fs::write(c.path.join("a.txt"), "one\nTWO\n").unwrap();
        std::fs::write(c.path.join("b.txt"), "bee\n").unwrap();
        assert_eq!(repo_root(&c.path).unwrap(), repo);

        let r = review(&c.path, "main").unwrap();
        assert!(r.dirty);
        assert_eq!(r.commits_ahead, 0);
        let paths: Vec<_> = r.files.iter().map(|f| (f.path.as_str(), f.status)).collect();
        assert_eq!(paths, vec![("a.txt", 'M'), ("b.txt", '?')]);
        assert!(r.diff_for("a.txt").contains("+TWO"));
        assert!(r.diff_for("b.txt").contains("+bee"));

        assert!(commit_all(&c.path, "hiero: add b").unwrap());
        let r = review(&c.path, "main").unwrap();
        assert_eq!((r.commits_ahead, r.dirty), (1, false));

        merge(&repo, "main", &c.branch, "Merge task: add b").unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap(), "bee\n");
        remove(&repo, &c.path, &c.branch).unwrap();
        assert!(!c.path.exists());
        assert!(!worktrees_home(&repo).exists());
        assert!(git(&repo, &["rev-parse", "--verify", "--quiet", "refs/heads/hiero/add-b"]).is_err());
    }

    #[test]
    fn merge_refuses_dirty_main() {
        let repo = temp_repo();
        let c = create(&repo, "x").unwrap();
        std::fs::write(c.path.join("x.txt"), "x\n").unwrap();
        commit_all(&c.path, "x").unwrap();
        std::fs::write(repo.join("a.txt"), "local edit\n").unwrap();
        let err = merge(&repo, "main", &c.branch, "m").unwrap_err();
        assert!(err.contains("uncommitted"), "{err}");
        remove(&repo, &c.path, &c.branch).unwrap();
    }
}
