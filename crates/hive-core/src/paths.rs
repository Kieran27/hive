//! Where hive keeps its files. Every location can be moved with `HIVE_HOME`
//! (handy for tests and for running a second instance).

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn hive_home() -> Option<PathBuf> {
    std::env::var_os("HIVE_HOME").map(PathBuf::from)
}

/// `~/.config/hive` — config.toml and projects/*.toml.
pub fn config_dir() -> PathBuf {
    hive_home()
        .map(|h| h.join("config"))
        .unwrap_or_else(|| home().join(".config/hive"))
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn project_config_dir() -> PathBuf {
    config_dir().join("projects")
}

/// `~/.local/state/hive` — database, logs, generated workspaces and hooks.
pub fn state_dir() -> PathBuf {
    hive_home()
        .map(|h| h.join("state"))
        .unwrap_or_else(|| home().join(".local/state/hive"))
}

pub fn db_file() -> PathBuf {
    state_dir().join("hive.db")
}

pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

pub fn workspaces_dir() -> PathBuf {
    state_dir().join("workspaces")
}

pub fn claude_settings_file() -> PathBuf {
    state_dir().join("claude-hooks.json")
}

/// The daemon socket. Unix socket paths are capped near 104 bytes on macOS,
/// so this stays short.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HIVE_SOCKET") {
        return PathBuf::from(p);
    }
    if let Some(h) = hive_home() {
        return h.join("daemon.sock");
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("hive/daemon.sock");
    }
    let uid = uid_string();
    PathBuf::from(format!("/tmp/hive-{uid}/daemon.sock"))
}

fn uid_string() -> String {
    std::env::var("UID").ok().unwrap_or_else(|| {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "user".into())
    })
}

/// Expand a leading `~/`.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(p)
}

/// Display a path with `$HOME` shortened to `~`.
pub fn tildify(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}
