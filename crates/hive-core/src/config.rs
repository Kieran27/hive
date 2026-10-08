//! Global config (`~/.config/hive/config.toml`) and per-project config
//! (`<repo>/.hive.toml`, else `~/.config/hive/projects/<name>.toml`).
//!
//! Everything is optional: a repo without any config still gets worktrees,
//! agents and shells, with packages and a `dev` run target auto-detected
//! from package.json.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::sanitize::{match_pattern, normalize_branch};

// ---------------------------------------------------------------- global

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GlobalConfig {
    /// Shell for terminals and for wrapping commands; defaults to `$SHELL`.
    pub shell: Option<String>,
    /// Extra project roots to show (projects added from the TUI live in the db).
    pub projects: Vec<String>,
    pub agents: AgentsConfig,
    pub keys: KeysConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentsConfig {
    pub claude: AgentCommand,
    pub codex: AgentCommand,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            claude: AgentCommand {
                cmd: "claude".into(),
                args: vec![],
            },
            codex: AgentCommand {
                cmd: "codex".into(),
                args: vec![],
            },
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentCommand {
    pub cmd: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct KeysConfig {
    /// Leaves terminal mode. `ctrl+<char>` or `ctrl+alt+<char>`-style names.
    pub unlock: String,
    /// Extra NAV bindings: action name → key, e.g. `claude = "C"`.
    /// Actions: claude codex shell run restart_run stop_run new_worktree
    /// setup remove_worktree add_project remove_project vscode close_tab
    /// resume next_attention palette help quit quit_stop_daemon reload.
    pub nav: BTreeMap<String, String>,
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            unlock: "ctrl+q".into(),
            nav: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UiConfig {
    pub sidebar_width: u16,
    /// macOS notification when an agent finishes or needs you.
    pub desktop_notifications: bool,
    pub mouse: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 34,
            desktop_notifications: false,
            mouse: true,
        }
    }
}

impl GlobalConfig {
    pub fn load() -> Result<Self> {
        let path = paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn shell(&self) -> String {
        self.shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/zsh".into())
    }
}

// ---------------------------------------------------------------- project file

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProjectFile {
    pub project: ProjectSection,
    pub package: Vec<PackageCfg>,
    pub node: NodeCfg,
    pub setup_step: Vec<StepCfg>,
    pub profile: Vec<ProfileCfg>,
    pub run: Vec<RunCfg>,
    pub vscode: VscodeCfg,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProjectSection {
    pub name: Option<String>,
    pub worktree_root: Option<String>,
    pub base_branch: Option<String>,
    pub port_stride: Option<u16>,
    /// Gitignored files (relative to repo root) copied from the main checkout
    /// into new worktrees.
    pub copy_from_main: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PackageCfg {
    pub name: String,
    pub path: String,
    /// Gitignored files (relative to the package) copied into new worktrees.
    pub copy_from_main: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NodeCfg {
    /// Wrap commands in `source nvm.sh; nvm use (.nvmrc | default_version)`.
    pub nvm: bool,
    pub default_version: String,
}

impl Default for NodeCfg {
    fn default() -> Self {
        Self {
            nvm: false,
            default_version: "20".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct StepCfg {
    pub id: String,
    pub label: Option<String>,
    /// `.` or a path relative to the worktree, or `@package`.
    pub cwd: Option<String>,
    pub cmd: String,
    /// Pin a node version for this step (`nvm use <node>`), ignoring .nvmrc.
    pub node: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProfileCfg {
    pub name: String,
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    pub env: Option<String>,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RunCfg {
    pub name: String,
    pub cwd: Option<String>,
    pub cmd: Option<String>,
    pub port: Option<u16>,
    pub env: BTreeMap<String, String>,
    /// Pin a node version for every proc of this target.
    pub node: Option<String>,
    pub proc: Vec<ProcCfg>,
    pub ask: Vec<AskCfg>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProcCfg {
    pub name: String,
    pub cwd: Option<String>,
    pub cmd: String,
    pub port: Option<u16>,
    /// Name of a sibling proc whose port must accept connections first.
    pub wait_for_port: Option<String>,
    pub env: BTreeMap<String, String>,
    pub node: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AskCfg {
    pub name: String,
    pub prompt: Option<String>,
    pub default: Option<String>,
    pub choices: Vec<String>,
    /// Shell command printing one choice per line.
    pub choices_cmd: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct VscodeCfg {
    /// Folders for the generated workspace (`.`, relative paths or `@package`).
    pub folders: Vec<String>,
}

impl Default for VscodeCfg {
    fn default() -> Self {
        Self {
            folders: vec![".".into()],
        }
    }
}

// ---------------------------------------------------------------- resolved

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub path: PathBuf,
    pub copy_from_main: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RunProc {
    pub name: String,
    pub cwd: Option<String>,
    pub cmd: String,
    pub port: Option<u16>,
    pub wait_for_port: Option<String>,
    pub env: BTreeMap<String, String>,
    pub node: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunTarget {
    pub name: String,
    pub procs: Vec<RunProc>,
    pub asks: Vec<AskCfg>,
}

/// A project's config with defaults applied.
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    pub name: String,
    pub root: PathBuf,
    pub worktree_root: PathBuf,
    pub base_branch: Option<String>,
    pub port_stride: u16,
    pub copy_from_main: Vec<String>,
    pub packages: Vec<Package>,
    pub node: NodeCfg,
    pub steps: Vec<StepCfg>,
    pub profiles: Vec<ProfileCfg>,
    pub runs: Vec<RunTarget>,
    pub vscode: VscodeCfg,
    pub source: Option<PathBuf>,
    pub error: Option<String>,
}

impl ProjectConfig {
    /// Load `<root>/.hive.toml`, falling back to the user-level file. A parse
    /// error is reported in `error` and defaults are used, so one bad file
    /// never hides a project.
    pub fn load(root: &Path) -> Self {
        let dir_name = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".into());
        let candidates = [
            root.join(".hive.toml"),
            paths::project_config_dir().join(format!("{dir_name}.toml")),
        ];
        let mut source = None;
        let mut error = None;
        let mut file = ProjectFile::default();
        for c in candidates {
            if let Ok(s) = std::fs::read_to_string(&c) {
                match toml::from_str::<ProjectFile>(&s) {
                    Ok(f) => file = f,
                    Err(e) => error = Some(format!("{}: {e}", c.display())),
                }
                source = Some(c);
                break;
            }
        }
        let mut cfg = Self::resolve(root, file);
        cfg.source = source;
        cfg.error = error;
        cfg
    }

    pub fn resolve(root: &Path, file: ProjectFile) -> Self {
        let dir_name = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".into());
        let name = file
            .project
            .name
            .clone()
            .unwrap_or_else(|| dir_name.clone());
        let worktree_root = file
            .project
            .worktree_root
            .as_deref()
            .map(|p| {
                let p = paths::expand_tilde(p);
                if p.is_relative() {
                    root.join(p)
                } else {
                    p
                }
            })
            .unwrap_or_else(|| {
                root.parent()
                    .unwrap_or(root)
                    .join(format!("{dir_name}-worktrees"))
            });

        let packages = if file.package.is_empty() {
            detect_packages(root)
        } else {
            file.package
                .iter()
                .map(|p| Package {
                    name: p.name.clone(),
                    path: PathBuf::from(&p.path),
                    copy_from_main: p.copy_from_main.clone(),
                })
                .collect()
        };

        let mut runs: Vec<RunTarget> = file.run.iter().map(resolve_run).collect();
        if file.run.is_empty() {
            if let Some(r) = detect_dev_run(root) {
                runs.push(r);
            }
        }

        Self {
            name,
            root: root.to_path_buf(),
            worktree_root,
            base_branch: file.project.base_branch.clone(),
            port_stride: file.project.port_stride.unwrap_or(10).max(1),
            copy_from_main: file.project.copy_from_main.clone(),
            packages,
            node: file.node.clone(),
            steps: file.setup_step.clone(),
            profiles: file.profile.clone(),
            runs,
            vscode: file.vscode.clone(),
            source: None,
            error: None,
        }
    }

    /// First profile whose pattern matches; else the last profile (the
    /// fallback rule portal-wt used); else none.
    pub fn profile_for_branch(&self, branch: &str) -> Option<&ProfileCfg> {
        let b = normalize_branch(branch);
        self.profiles
            .iter()
            .find(|p| p.patterns.iter().any(|pat| match_pattern(b, pat)))
            .or_else(|| self.profiles.last())
    }

    /// Resolve `.`, `@package` or a relative path against a worktree.
    pub fn resolve_dir(&self, worktree: &Path, spec: Option<&str>) -> PathBuf {
        match spec {
            None | Some(".") | Some("") => worktree.to_path_buf(),
            Some(s) if s.starts_with('@') => {
                let name = &s[1..];
                match self.packages.iter().find(|p| p.name == name) {
                    Some(p) => worktree.join(&p.path),
                    None => worktree.join(name),
                }
            }
            Some(s) => worktree.join(s),
        }
    }

    pub fn run(&self, name: &str) -> Option<&RunTarget> {
        self.runs.iter().find(|r| r.name == name)
    }

    pub fn step(&self, id: &str) -> Option<&StepCfg> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// `env:*` script suffixes offered for steps that use `{env}`.
    pub fn env_choices(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for step in self.steps.iter().filter(|s| s.cmd.contains("{env}")) {
            let dir = self.resolve_dir(&self.root, step.cwd.as_deref());
            for script in package_scripts(&dir) {
                if let Some(env) = script.strip_prefix("env:") {
                    if !out.iter().any(|e| e == env) {
                        out.push(env.to_string());
                    }
                }
            }
        }
        out
    }
}

fn resolve_run(r: &RunCfg) -> RunTarget {
    let procs = if r.proc.is_empty() {
        vec![RunProc {
            name: r.name.clone(),
            cwd: r.cwd.clone(),
            cmd: r.cmd.clone().unwrap_or_default(),
            port: r.port,
            wait_for_port: None,
            env: r.env.clone(),
            node: r.node.clone(),
        }]
    } else {
        r.proc
            .iter()
            .map(|p| {
                let mut env = r.env.clone();
                env.extend(p.env.clone());
                RunProc {
                    name: p.name.clone(),
                    cwd: p.cwd.clone().or_else(|| r.cwd.clone()),
                    cmd: p.cmd.clone(),
                    port: p.port,
                    wait_for_port: p.wait_for_port.clone(),
                    env,
                    node: p.node.clone().or_else(|| r.node.clone()),
                }
            })
            .collect()
    };
    RunTarget {
        name: r.name.clone(),
        procs,
        asks: r.ask.clone(),
    }
}

/// Script names from `<dir>/package.json`.
pub fn package_scripts(dir: &Path) -> Vec<String> {
    let Ok(s) = std::fs::read_to_string(dir.join("package.json")) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
        return vec![];
    };
    v.get("scripts")
        .and_then(|s| s.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Packages from package.json `workspaces` globs (only `dir/*` and literal
/// entries are expanded — enough for the common layouts).
fn detect_packages(root: &Path) -> Vec<Package> {
    let Ok(s) = std::fs::read_to_string(root.join("package.json")) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
        return vec![];
    };
    let globs: Vec<String> = match v.get("workspaces") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::Object(o)) => o
            .get("packages")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    };
    let mut out = Vec::new();
    for g in globs {
        if let Some(prefix) = g.strip_suffix("/*") {
            let Ok(rd) = std::fs::read_dir(root.join(prefix)) else {
                continue;
            };
            let mut dirs: Vec<_> = rd
                .flatten()
                .filter(|e| e.path().join("package.json").exists())
                .collect();
            dirs.sort_by_key(|e| e.file_name());
            for e in dirs {
                let name = e.file_name().to_string_lossy().to_string();
                out.push(Package {
                    name: name.clone(),
                    path: PathBuf::from(prefix).join(name),
                    copy_from_main: vec![],
                });
            }
        } else if root.join(&g).join("package.json").exists() {
            let name = Path::new(&g)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(g.clone());
            out.push(Package {
                name,
                path: PathBuf::from(&g),
                copy_from_main: vec![],
            });
        }
    }
    out
}

fn package_manager(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if root.join("yarn.lock").exists() {
        "yarn"
    } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
        "bun"
    } else {
        "npm"
    }
}

/// With no `[[run]]` configured, offer `<pm> run dev` (or `start`).
fn detect_dev_run(root: &Path) -> Option<RunTarget> {
    let scripts = package_scripts(root);
    let script = ["dev", "start"]
        .into_iter()
        .find(|s| scripts.iter().any(|x| x == s))?;
    let pm = package_manager(root);
    Some(RunTarget {
        name: script.to_string(),
        procs: vec![RunProc {
            name: script.to_string(),
            cwd: None,
            cmd: format!("{pm} run {script}"),
            port: None,
            wait_for_port: None,
            env: BTreeMap::new(),
            node: None,
        }],
        asks: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
[project]
name = "acme-app"
worktree_root = "/wt"
base_branch = "develop"

[[package]]
name = "mobile"
path = "apps/mobile"
copy_from_main = [".env.local"]

[[setup_step]]
id = "env"
cwd = "@mobile"
cmd = "yarn env:{env}"

[[profile]]
name = "native"
match = ["native/*", "native-*"]
steps = ["env"]

[[profile]]
name = "default"
match = ["*"]

[[run]]
name = "web"
env = { BROWSER = "none" }
[[run.proc]]
name = "api"
cwd = "apps/api"
port = 4000
cmd = "PORT={port} yarn start"
[[run.proc]]
name = "web"
port = 3000
wait_for_port = "api"
cmd = "yarn start"
env = { X = "1" }

[[run]]
name = "metro"
cwd = "@mobile"
port = 8081
cmd = "yarn start --port {port}"
node = "22"
"#;

    #[test]
    fn parses_and_resolves() {
        let f: ProjectFile = toml::from_str(FIXTURE).unwrap();
        let c = ProjectConfig::resolve(Path::new("/repo"), f);
        assert_eq!(c.name, "acme-app");
        assert_eq!(c.worktree_root, PathBuf::from("/wt"));
        assert_eq!(c.port_stride, 10);
        assert_eq!(c.runs.len(), 2);
        let web = c.run("web").unwrap();
        assert_eq!(web.procs.len(), 2);
        assert_eq!(
            web.procs[1].env.get("BROWSER").map(String::as_str),
            Some("none")
        );
        assert_eq!(web.procs[1].env.get("X").map(String::as_str), Some("1"));
        let metro = c.run("metro").unwrap();
        assert_eq!(metro.procs[0].port, Some(8081));
        assert_eq!(metro.procs[0].node.as_deref(), Some("22"));
        assert_eq!(web.procs[0].node, None);
        assert_eq!(
            c.resolve_dir(Path::new("/w"), Some("@mobile")),
            PathBuf::from("/w/apps/mobile")
        );
        assert_eq!(
            c.resolve_dir(Path::new("/w"), Some(".")),
            PathBuf::from("/w")
        );
    }

    #[test]
    fn bundled_example_parses() {
        let text = include_str!("../../../examples/web-and-mobile-monorepo.toml");
        let f: ProjectFile = toml::from_str(text).unwrap();
        let c = ProjectConfig::resolve(Path::new("/repo"), f);
        assert_eq!(c.packages.len(), 4);
        assert_eq!(
            c.run("web").unwrap().procs[1].wait_for_port.as_deref(),
            Some("store")
        );
        assert_eq!(c.run("ios").unwrap().procs[0].node.as_deref(), Some("22"));
        assert_eq!(
            c.profile_for_branch("native/cam").unwrap().steps,
            vec!["install-mobile", "env", "ios"]
        );
    }

    #[test]
    fn profile_precedence() {
        let f: ProjectFile = toml::from_str(FIXTURE).unwrap();
        let c = ProjectConfig::resolve(Path::new("/repo"), f);
        assert_eq!(
            c.profile_for_branch("native/camera").unwrap().name,
            "native"
        );
        assert_eq!(
            c.profile_for_branch("origin/native-x").unwrap().name,
            "native"
        );
        assert_eq!(c.profile_for_branch("feature/x").unwrap().name, "default");
    }

    #[test]
    fn defaults_without_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        std::fs::create_dir_all(root.join("packages/web")).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces":["packages/*"],"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("packages/web/package.json"), "{}").unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        let c = ProjectConfig::load(&root);
        assert_eq!(c.name, "app");
        assert_eq!(c.worktree_root, dir.path().join("app-worktrees"));
        assert_eq!(c.packages.len(), 1);
        assert_eq!(c.packages[0].path, PathBuf::from("packages/web"));
        assert_eq!(c.runs[0].procs[0].cmd, "pnpm run dev");
    }

    #[test]
    fn bad_config_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hive.toml"), "[project\n").unwrap();
        let c = ProjectConfig::load(dir.path());
        assert!(c.error.is_some());
    }
}
