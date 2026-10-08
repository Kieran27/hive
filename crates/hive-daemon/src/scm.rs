//! Source control for one worktree: status, stage/unstage, discard, diff.
//! (Commits run through the user's shell — see `Daemon::git_commit`.)

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use hive_core::protocol::{GitFile, GitStatusInfo};

/// Diffs bigger than this are cut off (the view is for reading, not patching).
const MAX_DIFF: usize = 2 * 1024 * 1024;

fn git_raw(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_raw(dir, args)?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git status --porcelain=v1 -z --branch`, parsed.
pub fn status(worktree: &Path) -> Result<GitStatusInfo> {
    let out = git(
        worktree,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
    )?;
    let mut info = parse_status(&out);
    info.worktree = worktree.to_path_buf();
    Ok(info)
}

pub fn parse_status(out: &str) -> GitStatusInfo {
    let mut info = GitStatusInfo::default();
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        if let Some(head) = entry.strip_prefix("## ") {
            parse_branch_line(head, &mut info);
            continue;
        }
        if entry.len() < 4 {
            continue;
        }
        let mut chars = entry.chars();
        let x = chars.next().unwrap();
        let y = chars.next().unwrap();
        let path = entry[3..].to_string();
        // Renames/copies carry the original path as the next NUL field.
        let orig_path = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            entries.next().map(String::from)
        } else {
            None
        };
        let side = |c: char| (c != ' ').then_some(c);
        let (staged, unstaged) = if x == '?' {
            (None, Some('?'))
        } else {
            (side(x), side(y))
        };
        if x == '!' {
            continue;
        }
        info.files.push(GitFile {
            path,
            orig_path,
            staged,
            unstaged,
        });
    }
    info.files.sort_by(|a, b| a.path.cmp(&b.path));
    info
}

fn parse_branch_line(head: &str, info: &mut GitStatusInfo) {
    // "main...origin/main [ahead 1, behind 2]", "No commits yet on main",
    // "HEAD (no branch)".
    let (names, counts) = match head.find(" [") {
        Some(i) => (&head[..i], Some(&head[i + 2..head.len().saturating_sub(1)])),
        None => (head, None),
    };
    if names.starts_with("HEAD (no branch)") {
        info.branch = None;
    } else if let Some(b) = names.strip_prefix("No commits yet on ") {
        info.branch = Some(b.to_string());
    } else if let Some((local, upstream)) = names.split_once("...") {
        info.branch = Some(local.to_string());
        info.upstream = Some(upstream.to_string());
    } else {
        info.branch = Some(names.to_string());
    }
    if let Some(counts) = counts {
        for part in counts.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                info.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                info.behind = n.parse().unwrap_or(0);
            }
        }
    }
}

fn with_paths<'a>(mut args: Vec<&'a str>, paths: &'a [String]) -> Vec<&'a str> {
    args.push("--");
    args.extend(paths.iter().map(String::as_str));
    args
}

pub fn stage(worktree: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    // `-A` so deletions are staged too.
    git(worktree, &with_paths(vec!["add", "-A"], paths)).map(|_| ())
}

pub fn unstage(worktree: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    // `reset` works before the first commit too, unlike `restore --staged`.
    let has_head = git_raw(worktree, &["rev-parse", "--verify", "--quiet", "HEAD"])?
        .status
        .success();
    if has_head {
        git(worktree, &with_paths(vec!["restore", "--staged"], paths)).map(|_| ())
    } else {
        git(
            worktree,
            &with_paths(vec!["rm", "--cached", "-r", "--quiet"], paths),
        )
        .map(|_| ())
    }
}

/// Drop unstaged changes: restore tracked files from the index and delete
/// untracked ones. Staged changes are kept.
pub fn discard(worktree: &Path, paths: &[String]) -> Result<()> {
    let st = status(worktree)?;
    let (untracked, tracked): (Vec<String>, Vec<String>) = paths
        .iter()
        .cloned()
        .partition(|p| st.files.iter().any(|f| &f.path == p && f.untracked()));
    if !tracked.is_empty() {
        git(
            worktree,
            &with_paths(vec!["restore", "--worktree"], &tracked),
        )?;
    }
    if !untracked.is_empty() {
        git(worktree, &with_paths(vec!["clean", "-f", "-q"], &untracked))?;
    }
    Ok(())
}

pub fn diff(worktree: &Path, path: &str, staged: bool) -> Result<String> {
    let st = status(worktree)?;
    let file = st.files.iter().find(|f| f.path == path);
    let out = if !staged && file.map(|f| f.untracked()).unwrap_or(false) {
        // New file: diff against nothing (exit code 1 means "differs").
        git_raw(
            worktree,
            &["diff", "--no-color", "--no-index", "--", "/dev/null", path],
        )?
        .stdout
    } else {
        let mut args = vec!["diff", "--no-color", "--find-renames"];
        if staged {
            args.push("--cached");
        }
        args.push("--");
        args.push(path);
        let out = git_raw(worktree, &args)?;
        if !out.status.success() {
            bail!("git diff: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        out.stdout
    };
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if text.len() > MAX_DIFF {
        let mut cut = MAX_DIFF;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… diff truncated …\n");
    }
    if text.trim().is_empty() {
        text = "(no textual changes — mode change or binary file)".into();
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let raw = "## feat/x...origin/feat/x [ahead 2, behind 1]\0M  staged.rs\0 M edited.rs\0MM both.rs\0?? new.txt\0R  new_name.rs\0old_name.rs\0 D gone.rs\0";
        let st = parse_status(raw);
        assert_eq!(st.branch.as_deref(), Some("feat/x"));
        assert_eq!(st.upstream.as_deref(), Some("origin/feat/x"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        let f = |p: &str| st.files.iter().find(|f| f.path == p).unwrap().clone();
        assert_eq!(
            (f("staged.rs").staged, f("staged.rs").unstaged),
            (Some('M'), None)
        );
        assert_eq!(
            (f("edited.rs").staged, f("edited.rs").unstaged),
            (None, Some('M'))
        );
        assert_eq!(
            (f("both.rs").staged, f("both.rs").unstaged),
            (Some('M'), Some('M'))
        );
        assert!(f("new.txt").untracked());
        assert_eq!(f("new_name.rs").orig_path.as_deref(), Some("old_name.rs"));
        assert_eq!(f("gone.rs").unstaged, Some('D'));
        assert_eq!(st.files.len(), 6);
    }

    #[test]
    fn branch_variants() {
        assert_eq!(
            parse_status("## No commits yet on main\0")
                .branch
                .as_deref(),
            Some("main")
        );
        assert_eq!(parse_status("## HEAD (no branch)\0").branch, None);
        let st = parse_status("## main\0");
        assert_eq!(
            (st.branch.as_deref(), st.upstream.as_deref()),
            (Some("main"), None)
        );
    }

    fn sh(dir: &Path, cmd: &str) {
        let o = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    #[test]
    fn stage_unstage_discard_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        sh(d, "git init -q -b main && printf 'a\\n' > keep.txt && printf 'x\\n' > tweak.txt && git add . && git -c user.email=a@b -c user.name=a commit -q -m init");
        sh(d, "printf 'a\\nb\\n' > keep.txt && printf 'local\\n' > tweak.txt && printf 'n\\n' > new.txt");
        let st = status(d).unwrap();
        assert_eq!(st.files.len(), 3);

        // Stage only the change we want; the local tweak stays unstaged.
        stage(d, &["keep.txt".into(), "new.txt".into()]).unwrap();
        let st = status(d).unwrap();
        let get = |p: &str| st.files.iter().find(|f| f.path == p).unwrap().clone();
        assert_eq!(get("keep.txt").staged, Some('M'));
        assert_eq!(get("new.txt").staged, Some('A'));
        assert_eq!(
            (get("tweak.txt").staged, get("tweak.txt").unstaged),
            (None, Some('M'))
        );

        assert!(diff(d, "keep.txt", true).unwrap().contains("+b"));
        assert!(diff(d, "tweak.txt", false).unwrap().contains("+local"));

        unstage(d, &["new.txt".into()]).unwrap();
        assert!(status(d)
            .unwrap()
            .files
            .iter()
            .any(|f| f.path == "new.txt" && f.untracked()));
        assert!(diff(d, "new.txt", false).unwrap().contains("+n"));

        discard(d, &["tweak.txt".into(), "new.txt".into()]).unwrap();
        let st = status(d).unwrap();
        assert_eq!(st.files.len(), 1, "{:?}", st.files);
        assert!(!d.join("new.txt").exists());
        assert_eq!(std::fs::read_to_string(d.join("tweak.txt")).unwrap(), "x\n");
    }

    #[test]
    fn unstage_before_first_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        sh(d, "git init -q -b main && echo a > f && git add f");
        unstage(d, &["f".into()]).unwrap();
        assert!(status(d).unwrap().files[0].untracked());
    }
}
