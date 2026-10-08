//! Git worktree operations (ported from portal-worktree-tui `git.ts`).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use hive_core::protocol::WorktreeMode;
use hive_core::sanitize::sanitize_branch_name;

#[derive(Debug, Clone, PartialEq)]
pub struct RawWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub prunable: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Polling must never take .git/index.lock, or it would race the
        // user's own git commands (and ours).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Top-level of the main checkout containing `path`.
pub fn main_root(path: &Path) -> Result<PathBuf> {
    let common = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common.trim());
    if common.file_name().map(|n| n == ".git").unwrap_or(false) {
        Ok(common.parent().unwrap().to_path_buf())
    } else {
        // Bare repo or unusual layout: fall back to the toplevel.
        Ok(PathBuf::from(
            git(path, &["rev-parse", "--show-toplevel"])?.trim(),
        ))
    }
}

pub fn parse_porcelain(text: &str) -> Vec<RawWorktree> {
    let mut out = Vec::new();
    let mut cur: Option<RawWorktree> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            cur = Some(RawWorktree {
                path: PathBuf::from(p),
                head: None,
                branch: None,
                bare: false,
                prunable: false,
            });
        } else if let Some(w) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                w.head = Some(h.to_string());
            } else if let Some(b) = line.strip_prefix("branch ") {
                w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if line == "bare" {
                w.bare = true;
            } else if line.starts_with("prunable") {
                w.prunable = true;
            }
        }
    }
    if let Some(w) = cur {
        out.push(w);
    }
    out
}

pub fn list_worktrees(repo: &Path) -> Result<Vec<RawWorktree>> {
    Ok(
        parse_porcelain(&git(repo, &["worktree", "list", "--porcelain"])?)
            .into_iter()
            .filter(|w| !w.bare)
            .collect(),
    )
}

pub fn is_dirty(path: &Path) -> bool {
    git(path, &["status", "--porcelain", "--ignore-submodules"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

pub fn default_branch(repo: &Path) -> String {
    if let Ok(s) = git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(b) = s.trim().strip_prefix("origin/") {
            return b.to_string();
        }
    }
    for b in ["main", "master", "develop"] {
        if git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{b}"),
            ],
        )
        .is_ok()
        {
            return b.to_string();
        }
    }
    "main".into()
}

pub fn branches(repo: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let local = git(
        repo,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ],
    )?;
    let remote = git(
        repo,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/remotes",
        ],
    )?;
    Ok((
        local.lines().map(String::from).collect(),
        remote
            .lines()
            .filter(|l| !l.ends_with("/HEAD") && l.contains('/'))
            .map(String::from)
            .collect(),
    ))
}

pub fn fetch(repo: &Path) -> Result<()> {
    git(repo, &["fetch", "--all", "--prune"]).map(|_| ())
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// Default destination: `<worktree_root>/<sanitized branch>`.
pub fn default_dest(worktree_root: &Path, branch: &str) -> PathBuf {
    worktree_root.join(sanitize_branch_name(branch))
}

/// Create a worktree; returns its path and branch.
pub fn add_worktree(
    repo: &Path,
    worktree_root: &Path,
    mode: WorktreeMode,
    branch: &str,
    base: Option<&str>,
) -> Result<(PathBuf, Option<String>)> {
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("branch name is empty");
    }
    let dest = default_dest(worktree_root, branch);
    if dest.exists()
        && std::fs::read_dir(&dest)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        bail!("{} already exists and is not empty", dest.display());
    }
    std::fs::create_dir_all(worktree_root)?;
    let d = dest.display().to_string();
    let local_branch = match mode {
        WorktreeMode::NewBranch => {
            let base = base
                .filter(|b| !b.is_empty())
                .map(String::from)
                .unwrap_or_else(|| default_branch(repo));
            git(repo, &["worktree", "add", "-b", branch, &d, &base])?;
            Some(branch.to_string())
        }
        WorktreeMode::Local => {
            git(repo, &["worktree", "add", &d, branch])?;
            Some(branch.to_string())
        }
        WorktreeMode::Remote => {
            let local = branch
                .split_once('/')
                .map(|(_, b)| b)
                .unwrap_or(branch)
                .to_string();
            if branch_exists(repo, &local) {
                // Reuse the existing local branch (portal-wt asked; we just do it).
                git(repo, &["worktree", "add", &d, &local])?;
            } else {
                git(
                    repo,
                    &["worktree", "add", "--track", "-b", &local, &d, branch],
                )?;
            }
            Some(local)
        }
        WorktreeMode::Detached => {
            git(repo, &["worktree", "add", "--detach", &d, branch])?;
            None
        }
    };
    Ok((dest, local_branch))
}

pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<()> {
    if same_path(repo, path) {
        bail!("refusing to remove the main checkout");
    }
    if !path.exists() {
        // A worktree whose directory is gone ("prunable"): just prune it.
        git(repo, &["worktree", "prune"])?;
        return Ok(());
    }
    let p = path.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&p);
    git(repo, &args)?;
    let _ = git(repo, &["worktree", "prune"]);
    Ok(())
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain() {
        let text = "worktree /r\nHEAD abc\nbranch refs/heads/develop\n\nworktree /w/x\nHEAD def\ndetached\n\nworktree /w/gone\nHEAD 123\nbranch refs/heads/feature/a\nprunable gitdir file points to non-existent location\n";
        let w = parse_porcelain(text);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].branch.as_deref(), Some("develop"));
        assert_eq!(w[1].branch, None);
        assert_eq!(w[1].head.as_deref(), Some("def"));
        assert!(w[2].prunable);
        assert_eq!(w[2].branch.as_deref(), Some("feature/a"));
    }

    fn sh(dir: &Path, cmd: &str) {
        let ok = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    #[test]
    fn add_list_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        sh(&repo, "git init -q -b main && git -c user.email=a@b -c user.name=a commit -q --allow-empty -m init");
        let root = tmp.path().join("wts");
        let (dest, b) =
            add_worktree(&repo, &root, WorktreeMode::NewBranch, "feature/x", None).unwrap();
        assert_eq!(b.as_deref(), Some("feature/x"));
        assert!(dest.ends_with("feature-x"));
        let list = list_worktrees(&repo).unwrap();
        assert_eq!(list.len(), 2);
        assert!(same_path(&main_root(&dest).unwrap(), &repo));
        assert!(remove_worktree(&repo, &repo, false).is_err());
        std::fs::write(dest.join("f"), "x").unwrap();
        assert!(is_dirty(&dest));
        assert!(remove_worktree(&repo, &dest, false).is_err());
        remove_worktree(&repo, &dest, true).unwrap();
        assert_eq!(list_worktrees(&repo).unwrap().len(), 1);
        let (d2, _) = add_worktree(&repo, &root, WorktreeMode::Detached, "main", None).unwrap();
        assert!(list_worktrees(&repo)
            .unwrap()
            .iter()
            .any(|w| same_path(&w.path, &d2) && w.branch.is_none()));
    }
}
