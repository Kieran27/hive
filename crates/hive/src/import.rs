//! `hive init` (starter project config) and `hive import-portal-wt`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

fn q(s: &str) -> String {
    toml_string(s)
}

fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn list(items: &[String]) -> String {
    format!(
        "[{}]",
        items.iter().map(|s| q(s)).collect::<Vec<_>>().join(", ")
    )
}

fn write_new(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        bail!(
            "{} already exists — edit it, or delete it first",
            path.display()
        );
    }
    // Validate before writing.
    toml::from_str::<hive_core::config::ProjectFile>(contents)
        .context("generated config does not parse (bug)")?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, contents)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn user_config_path(root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());
    hive_core::paths::project_config_dir().join(format!("{name}.toml"))
}

pub fn init(root: &Path, in_repo: bool) -> Result<()> {
    let cfg = hive_core::config::ProjectConfig::resolve(root, Default::default());
    let mut s = String::new();
    writeln!(
        s,
        "# hive project config — every key is documented in the hive README."
    )?;
    writeln!(s, "[project]")?;
    writeln!(s, "name = {}", q(&cfg.name))?;
    writeln!(
        s,
        "worktree_root = {}",
        q(&hive_core::paths::tildify(&cfg.worktree_root))
    )?;
    writeln!(s, "# base_branch = \"main\"        # default: origin/HEAD")?;
    writeln!(
        s,
        "port_stride = 10              # worktree slot N runs on base + N*stride"
    )?;
    writeln!(s, "copy_from_main = []           # gitignored files to copy into new worktrees, e.g. [\".env.local\"]")?;
    writeln!(s)?;
    writeln!(s, "[node]")?;
    writeln!(
        s,
        "nvm = {}",
        hive_core::paths::expand_tilde("~/.nvm/nvm.sh").exists()
            && root.join("package.json").exists()
    )?;
    writeln!(s, "default_version = \"20\"")?;
    for p in &cfg.packages {
        writeln!(
            s,
            "\n[[package]]\nname = {}\npath = {}",
            q(&p.name),
            q(&p.path.display().to_string())
        )?;
    }
    if root.join("package.json").exists() {
        let pm = if root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else {
            "npm"
        };
        writeln!(
            s,
            "\n[[setup_step]]\nid = \"install\"\ncmd = \"{pm} install\""
        )?;
        writeln!(
            s,
            "\n[[profile]]\nname = \"default\"\nmatch = [\"*\"]\nsteps = [\"install\"]"
        )?;
    }
    for r in &cfg.runs {
        let p = &r.procs[0];
        writeln!(
            s,
            "\n[[run]]\nname = {}\ncmd = {}\n# port = 3000             # then use {{port}} in cmd",
            q(&r.name),
            q(&p.cmd)
        )?;
    }
    if cfg.runs.is_empty() {
        writeln!(
            s,
            "\n# [[run]]\n# name = \"dev\"\n# port = 3000\n# cmd = \"PORT={{port}} npm run dev\""
        )?;
    }
    let path = if in_repo {
        root.join(".hive.toml")
    } else {
        user_config_path(root)
    };
    write_new(&path, &s)
}

// ---------------------------------------------------------------- portal-wt

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct PortalWtConfig {
    repo_root: Option<String>,
    app_subdir: Option<String>,
    worktree_root: Option<String>,
    default_base_branch: Option<String>,
    profiles: Vec<PortalWtProfile>,
    commands: std::collections::BTreeMap<String, PortalWtCommand>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct PortalWtProfile {
    name: String,
    #[serde(rename = "match")]
    patterns: Vec<String>,
    env_script: Option<String>,
    post_create_actions: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PortalWtCommand {
    command: String,
    args: Vec<String>,
}

/// portal-wt's built-in branch profiles, used when no config file exists or
/// it lists none. Repo, app and worktree paths have no defaults: they come
/// from the config file or the `--repo` / `--app` flags.
fn portal_wt_defaults() -> PortalWtConfig {
    let p = |name: &str, m: &[&str], env: &str, actions: &[&str]| PortalWtProfile {
        name: name.into(),
        patterns: m.iter().map(|s| s.to_string()).collect(),
        env_script: Some(env.into()),
        post_create_actions: actions.iter().map(|s| s.to_string()).collect(),
    };
    PortalWtConfig {
        repo_root: None,
        app_subdir: None,
        worktree_root: None,
        default_base_branch: Some("develop".into()),
        profiles: vec![
            p(
                "develop",
                &[
                    "develop",
                    "feature/*",
                    "fix/*",
                    "refactor/*",
                    "chore/*",
                    "hotfix/*",
                ],
                "env:staging",
                &["install", "env"],
            ),
            p(
                "release",
                &["release/*"],
                "env:staging",
                &["install", "env"],
            ),
            p(
                "native",
                &["native/*", "native-*"],
                "env:staging",
                &["install", "env", "ios"],
            ),
            p(
                "master",
                &["master", "main"],
                "env:prod",
                &["install", "env"],
            ),
            p("default", &["*"], "env:staging", &["install", "env"]),
        ],
        commands: Default::default(),
    }
}

pub fn import_portal_wt(
    repo: Option<PathBuf>,
    app: Option<String>,
    config: Option<PathBuf>,
) -> Result<()> {
    let config_path =
        config.unwrap_or_else(|| hive_core::paths::expand_tilde("~/.config/portal-wt/config.json"));
    let mut c = match std::fs::read_to_string(&config_path) {
        Ok(s) => {
            println!("reading {}", config_path.display());
            serde_json::from_str::<PortalWtConfig>(&s)
                .with_context(|| format!("parsing {}", config_path.display()))?
        }
        Err(_) => {
            println!(
                "{} not found — using portal-wt's built-in defaults",
                config_path.display()
            );
            portal_wt_defaults()
        }
    };
    let defaults = portal_wt_defaults();
    if c.profiles.is_empty() {
        c.profiles = defaults.profiles;
    }
    let repo = match repo.or_else(|| c.repo_root.as_deref().map(hive_core::paths::expand_tilde)) {
        Some(r) => r,
        None => bail!("no repoRoot in the portal-wt config — pass --repo <path>"),
    };
    let repo = std::fs::canonicalize(&repo)
        .with_context(|| format!("repo {} not found — pass --repo <path>", repo.display()))?;
    let Some(app) = app.or_else(|| c.app_subdir.clone()) else {
        bail!("no appSubdir in the portal-wt config — pass --app <path relative to the repo>")
    };
    let app_name = Path::new(&app)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".into());
    let wt_root = c
        .worktree_root
        .clone()
        .map(|w| hive_core::paths::expand_tilde(&w))
        .unwrap_or_else(|| {
            repo.parent().unwrap().join(format!(
                "{}-worktrees",
                repo.file_name().unwrap().to_string_lossy()
            ))
        });
    let cmd = |name: &str, fallback: &str| -> String {
        c.commands
            .get(name)
            .map(|k| {
                std::iter::once(k.command.clone())
                    .chain(k.args.clone())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_else(|| fallback.into())
    };

    let mut s = String::new();
    writeln!(
        s,
        "# Imported from portal-worktree-tui by `hive import-portal-wt`."
    )?;
    writeln!(s, "[project]")?;
    writeln!(
        s,
        "name = {}",
        q(&repo.file_name().unwrap().to_string_lossy())
    )?;
    writeln!(
        s,
        "worktree_root = {}",
        q(&hive_core::paths::tildify(&wt_root))
    )?;
    writeln!(
        s,
        "base_branch = {}",
        q(c.default_base_branch.as_deref().unwrap_or("develop"))
    )?;
    writeln!(s, "port_stride = 10")?;
    writeln!(s, "\n[node]\nnvm = true\ndefault_version = \"20\"")?;
    writeln!(
        s,
        "\n[[package]]\nname = {}\npath = {}\ncopy_from_main = [\".env.local\"]",
        q(&app_name),
        q(&app)
    )?;
    writeln!(s, "\n[vscode]\nfolders = [{}]", q(&format!("@{app_name}")))?;
    let at = format!("@{app_name}");
    for (id, command) in [
        ("install", cmd("install", "yarn install")),
        ("env", "yarn env:{env}".to_string()),
        ("ios", cmd("ios", "yarn ios")),
        ("android", cmd("android", "yarn android")),
    ] {
        writeln!(
            s,
            "\n[[setup_step]]\nid = {}\ncwd = {}\ncmd = {}",
            q(id),
            q(&at),
            q(&command)
        )?;
    }
    for p in &c.profiles {
        let env = p
            .env_script
            .as_deref()
            .map(|e| e.trim_start_matches("env:").to_string());
        let steps: Vec<String> = p
            .post_create_actions
            .iter()
            .filter(|a| a.as_str() != "start")
            .cloned()
            .collect();
        writeln!(
            s,
            "\n[[profile]]\nname = {}\nmatch = {}",
            q(&p.name),
            list(&p.patterns)
        )?;
        if let Some(e) = env {
            writeln!(s, "env = {}", q(&e))?;
        }
        writeln!(s, "steps = {}", list(&steps))?;
    }
    writeln!(
        s,
        r#"
[[run]]
name = "metro"
cwd = "@{app_name}"
port = 8081
cmd = "npx expo start --dev-client --port {{port}}"

[[run]]
name = "ios"
cwd = "@{app_name}"
port = 8081
cmd = "npx expo run:ios --device \"{{device}}\" --port {{port}}"
[[run.ask]]
name = "device"
prompt = "iOS simulator"
choices_cmd = "xcrun simctl list devices available | grep -E '^ +iPhone|^ +iPad' | sed -E 's/^ +//; s/ \\([0-9A-F-]{{36}}\\).*//'"

[[run]]
name = "android"
cwd = "@{app_name}"
port = 8081
cmd = "npx expo run:android --device \"{{device}}\" --port {{port}}"
[[run.ask]]
name = "device"
prompt = "Android device / emulator"
choices_cmd = "emulator -list-avds 2>/dev/null; adb devices 2>/dev/null | tail -n +2 | cut -f1"
"#
    )?;
    let path = user_config_path(&repo);
    write_new(&path, &s)?;
    println!(
        "next: `hive add {}` (or press `a` in the TUI)",
        hive_core::paths::tildify(&repo)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_config_parses() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HIVE_HOME", tmp.path());
        let repo = tmp.path().join("acme-app");
        std::fs::create_dir_all(repo.join("apps/mobile")).unwrap();
        let cfg = tmp.path().join("none.json");
        // Without a config file, the app path must be given.
        assert!(import_portal_wt(Some(repo.clone()), None, Some(cfg.clone())).is_err());
        import_portal_wt(Some(repo.clone()), Some("apps/mobile".into()), Some(cfg)).unwrap();
        let written =
            std::fs::read_to_string(tmp.path().join("config/projects/acme-app.toml")).unwrap();
        let file: hive_core::config::ProjectFile = toml::from_str(&written).unwrap();
        let resolved = hive_core::config::ProjectConfig::resolve(&repo, file);
        assert_eq!(
            resolved.profile_for_branch("native/x").unwrap().steps,
            vec!["install", "env", "ios"]
        );
        assert_eq!(
            resolved
                .profile_for_branch("master")
                .unwrap()
                .env
                .as_deref(),
            Some("prod")
        );
        assert_eq!(resolved.run("ios").unwrap().asks[0].name, "device");
        assert_eq!(resolved.packages[0].name, "mobile");
        assert_eq!(resolved.packages[0].copy_from_main, vec![".env.local"]);
    }
}
