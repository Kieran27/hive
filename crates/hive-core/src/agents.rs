//! How to launch, resume and hook each agent CLI.
//!
//! Status flows back through `hive notify`, which agents run from their hook
//! mechanisms with `HIVE_SESSION_ID` in the environment:
//! - claude: `--settings <state>/claude-hooks.json` adds hook commands without
//!   touching any file in the repo;
//! - codex: `-c notify=[…]` per launch, so `~/.codex/config.toml` is untouched.

use std::path::Path;

use serde_json::json;

use crate::config::AgentCommand;
use crate::protocol::SessionKind;

/// Marker present in every hook command hive writes.
pub const HOOK_MARKER: &str = "hive-managed-hook";

/// Claude hook events hive listens to.
pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "Notification",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

/// The settings JSON passed with `claude --settings`.
pub fn claude_hook_settings() -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    for ev in CLAUDE_EVENTS {
        let cmd = format!(
            "[ -n \"$HIVE_BIN\" ] && [ -n \"$HIVE_SESSION_ID\" ] && \"$HIVE_BIN\" notify --agent claude --event {ev} || true # {HOOK_MARKER}"
        );
        hooks.insert(
            ev.to_string(),
            json!([{ "matcher": "", "hooks": [{ "type": "command", "command": cmd, "timeout": 10 }] }]),
        );
    }
    json!({ "hooks": hooks })
}

/// Argv to start an agent (not yet wrapped in a shell).
pub fn launch_argv(
    kind: &SessionKind,
    cmd: &AgentCommand,
    hive_bin: &Path,
    claude_settings: &Path,
    resume: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![cmd.cmd.clone()];
    match kind {
        SessionKind::Claude => {
            argv.extend(cmd.args.iter().cloned());
            if let Some(id) = resume {
                argv.push("--resume".into());
                argv.push(id.into());
            }
            argv.push("--settings".into());
            argv.push(claude_settings.display().to_string());
        }
        SessionKind::Codex => {
            if let Some(id) = resume {
                argv.push("resume".into());
                argv.push(id.into());
            }
            argv.extend(cmd.args.iter().cloned());
            argv.push("-c".into());
            argv.push(codex_notify_override(hive_bin));
        }
        _ => {}
    }
    argv
}

/// `notify=["/path/hive","notify","--agent","codex"]`; codex appends the JSON
/// payload as the last argument.
pub fn codex_notify_override(hive_bin: &Path) -> String {
    let arr = json!([
        hive_bin.display().to_string(),
        "notify",
        "--agent",
        "codex",
        "--event",
        "notify"
    ]);
    format!("notify={arr}")
}

/// The agent's own session id from a hook payload.
pub fn agent_session_id(agent: &str, payload: &serde_json::Value) -> Option<String> {
    let key = if agent == "codex" {
        "thread-id"
    } else {
        "session_id"
    };
    payload.get(key).and_then(|v| v.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_argv() {
        let c = AgentCommand {
            cmd: "claude".into(),
            args: vec!["--model".into(), "opus".into()],
        };
        let a = launch_argv(
            &SessionKind::Claude,
            &c,
            Path::new("/bin/hive"),
            Path::new("/s.json"),
            Some("abc"),
        );
        assert_eq!(
            a,
            [
                "claude",
                "--model",
                "opus",
                "--resume",
                "abc",
                "--settings",
                "/s.json"
            ]
        );
    }

    #[test]
    fn codex_argv() {
        let c = AgentCommand {
            cmd: "codex".into(),
            args: vec![],
        };
        let a = launch_argv(
            &SessionKind::Codex,
            &c,
            Path::new("/bin/hive"),
            Path::new("/s.json"),
            Some("t1"),
        );
        assert_eq!(a[..3], ["codex", "resume", "t1"]);
        assert_eq!(a[3], "-c");
        assert!(a[4].starts_with("notify=[\"/bin/hive\",\"notify\""));
    }

    #[test]
    fn hook_settings_cover_events() {
        let v = claude_hook_settings();
        for ev in CLAUDE_EVENTS {
            let cmd = v["hooks"][ev][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.contains(HOOK_MARKER) && cmd.contains(ev));
        }
    }

    #[test]
    fn session_ids() {
        assert_eq!(
            agent_session_id("claude", &json!({"session_id":"s"})).as_deref(),
            Some("s")
        );
        assert_eq!(
            agent_session_id("codex", &json!({"thread-id":"t"})).as_deref(),
            Some("t")
        );
    }
}
