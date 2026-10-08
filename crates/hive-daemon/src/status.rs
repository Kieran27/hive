//! Agent hook events → session status.

use hive_core::protocol::Status;
use serde_json::Value;

#[derive(Debug, Default, PartialEq)]
pub struct HookUpdate {
    pub status: Option<Status>,
    pub agent_session: Option<String>,
    pub prompt: Option<String>,
}

pub fn interpret(agent: &str, event: &str, payload: &Value, current: Status) -> HookUpdate {
    let agent_session = hive_core::agents::agent_session_id(agent, payload);
    let mut u = HookUpdate {
        agent_session,
        ..Default::default()
    };
    if agent == "codex" {
        if payload.get("type").and_then(Value::as_str) == Some("agent-turn-complete") {
            u.status = Some(Status::Done);
            u.prompt = payload
                .get("input-messages")
                .and_then(Value::as_array)
                .and_then(|a| a.last())
                .and_then(Value::as_str)
                .map(String::from);
        }
        return u;
    }
    u.status = match event {
        "SessionStart" if current != Status::Working => Some(Status::Idle),
        "UserPromptSubmit" => {
            u.prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .map(String::from);
            Some(Status::Working)
        }
        "PostToolUse" => Some(Status::Working),
        "PermissionRequest" => Some(Status::Waiting),
        "Notification" => {
            let kind = payload
                .get("notification_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            let msg = payload.get("message").and_then(Value::as_str).unwrap_or("");
            let idle = kind == "idle_prompt" || msg.contains("waiting for your input");
            if idle && current == Status::Done {
                None
            } else {
                Some(Status::Waiting)
            }
        }
        "Stop" => Some(Status::Done),
        "SessionEnd" => Some(Status::Idle),
        _ => None,
    };
    u
}

/// A one-line title from a prompt.
pub fn title_from_prompt(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut t: String = line.chars().take(48).collect();
    if line.chars().count() > 48 {
        t.push('…');
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_flow() {
        let u = interpret(
            "claude",
            "UserPromptSubmit",
            &json!({"session_id":"s","prompt":"fix it"}),
            Status::Idle,
        );
        assert_eq!(u.status, Some(Status::Working));
        assert_eq!(u.agent_session.as_deref(), Some("s"));
        assert_eq!(u.prompt.as_deref(), Some("fix it"));
        assert_eq!(
            interpret(
                "claude",
                "Notification",
                &json!({"message":"Claude needs your permission to use Bash"}),
                Status::Working
            )
            .status,
            Some(Status::Waiting)
        );
        assert_eq!(
            interpret("claude", "PostToolUse", &json!({}), Status::Waiting).status,
            Some(Status::Working)
        );
        assert_eq!(
            interpret("claude", "Stop", &json!({}), Status::Working).status,
            Some(Status::Done)
        );
        // Idle reminder after finishing doesn't downgrade "done".
        assert_eq!(
            interpret(
                "claude",
                "Notification",
                &json!({"notification_type":"idle_prompt"}),
                Status::Done
            )
            .status,
            None
        );
    }

    #[test]
    fn codex_turn_complete() {
        let u = interpret(
            "codex",
            "notify",
            &json!({"type":"agent-turn-complete","thread-id":"t","input-messages":["a","b"]}),
            Status::Working,
        );
        assert_eq!(u.status, Some(Status::Done));
        assert_eq!(u.agent_session.as_deref(), Some("t"));
        assert_eq!(u.prompt.as_deref(), Some("b"));
    }

    #[test]
    fn titles() {
        assert_eq!(title_from_prompt("\n  hello world\nmore"), "hello world");
        assert_eq!(title_from_prompt(&"x".repeat(60)).chars().count(), 49);
    }
}
