//! One VS Code window per worktree: a generated `.code-workspace` whose
//! window title leads with the branch. Opening the same workspace again
//! focuses the existing window instead of opening another.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hive_core::config::ProjectConfig;
use hive_core::sanitize::sanitize_branch_name;
use serde_json::json;

pub fn workspace_file(project: &str, label: &str) -> PathBuf {
    hive_core::paths::workspaces_dir().join(format!(
        "{}.code-workspace",
        sanitize_branch_name(&format!("{project}-{label}"))
    ))
}

pub fn workspace_contents(cfg: &ProjectConfig, worktree: &Path, label: &str) -> serde_json::Value {
    let folders: Vec<_> = cfg
        .vscode
        .folders
        .iter()
        .map(|f| {
            let p = cfg.resolve_dir(worktree, Some(f));
            let name = if f == "." {
                format!("{} «{label}»", cfg.name)
            } else {
                format!("{} «{label}»", f.trim_start_matches('@'))
            };
            json!({ "name": name, "path": p })
        })
        .collect();
    json!({
        "folders": folders,
        "settings": {
            "window.title": format!("«{label}» ${{dirty}}${{activeEditorShort}}${{separator}}${{rootName}}")
        }
    })
}

pub fn open(cfg: &ProjectConfig, worktree: &Path, label: &str) -> Result<PathBuf> {
    let file = workspace_file(&cfg.name, label);
    std::fs::create_dir_all(file.parent().unwrap())?;
    let contents = serde_json::to_string_pretty(&workspace_contents(cfg, worktree, label))?;
    // Rewrite only on change so VS Code doesn't see a modified workspace.
    if std::fs::read_to_string(&file).ok().as_deref() != Some(contents.as_str()) {
        std::fs::write(&file, contents)?;
    }
    let code = which_code();
    std::process::Command::new(&code)
        .arg(&file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("launching {code}"))?;
    Ok(file)
}

fn which_code() -> String {
    for c in ["/usr/local/bin/code", "/opt/homebrew/bin/code"] {
        if Path::new(c).exists() {
            return c.into();
        }
    }
    "code".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_core::config::ProjectFile;

    #[test]
    fn title_and_folders() {
        let file: ProjectFile = toml::from_str("[vscode]\nfolders = [\".\", \"@rn\"]\n[[package]]\nname = \"rn\"\npath = \"apps/rn\"\n").unwrap();
        let cfg = ProjectConfig::resolve(Path::new("/repo"), file);
        let v = workspace_contents(&cfg, Path::new("/w/feat"), "feat/x");
        assert_eq!(v["folders"][1]["path"], "/w/feat/apps/rn");
        assert!(v["settings"]["window.title"]
            .as_str()
            .unwrap()
            .starts_with("«feat/x» "));
        assert!(workspace_file("p", "feat/x").ends_with("p-feat-x.code-workspace"));
    }
}
