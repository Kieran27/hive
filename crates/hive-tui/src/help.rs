//! Key glossary (the `?` screen) and the context-aware hints in the status
//! bar. One table feeds both, so they never disagree.

use hive_core::protocol::Status;

use crate::app::{App, Mode, Row, Tab};
use crate::overlay::Overlay;

pub struct Entry {
    pub section: &'static str,
    pub keys: &'static str,
    pub desc: &'static str,
}

const fn e(section: &'static str, keys: &'static str, desc: &'static str) -> Entry {
    Entry {
        section,
        keys,
        desc,
    }
}

/// `{unlock}` is replaced with the configured unlock key.
pub const GLOSSARY: &[Entry] = &[
    e(
        "modes",
        "{unlock}",
        "leave TERM mode (keys go to hive again)",
    ),
    e(
        "modes",
        "enter / click a pane",
        "enter TERM mode (keys go to the terminal)",
    ),
    e("navigate", "j k  ↑ ↓", "move in the sidebar"),
    e("navigate", "h l  ← →  space", "collapse / expand"),
    e("navigate", "home  G", "top / bottom of the sidebar"),
    e("navigate", "1-9  tab  shift+tab", "switch session tabs"),
    e("navigate", "[  ]", "switch process inside a run tab"),
    e(
        "navigate",
        ".",
        "jump to the next session waiting for you or done",
    ),
    e("navigate", "/", "fuzzy-jump to any worktree or session"),
    e("navigate", "pgup  pgdn", "scroll the terminal back"),
    e("navigate", "<  >  (or drag the border)", "sidebar width"),
    e(
        "sessions",
        "c",
        "new Claude Code session (in the selected package)",
    ),
    e("sessions", "x", "new Codex session"),
    e("sessions", "t", "new shell"),
    e("sessions", "w", "close the tab (kills its process)"),
    e(
        "sessions",
        "u",
        "resume an ended agent / restart an ended shell",
    ),
    e(
        "sessions",
        "e",
        "open or focus this worktree's VS Code window",
    ),
    e("runs", "r", "run a target (dev servers, metro, ios…)"),
    e("runs", "R", "restart the run in the current tab"),
    e("runs", "s", "stop the run in the current tab"),
    e("worktrees", "n", "new worktree (+ setup steps)"),
    e("worktrees", "S", "re-run setup steps"),
    e("worktrees", "D", "remove (or prune a missing) worktree"),
    e("worktrees", "a  X", "add / remove a project"),
    e("worktrees", "ctrl+r", "reload config files"),
    e("source control", "g", "open / focus / hide the panel"),
    e(
        "source control",
        "space",
        "stage or unstage a file (on a header: the whole list)",
    ),
    e("source control", "a  u", "stage all / unstage all"),
    e(
        "source control",
        "enter  (or click twice)",
        "show the file's diff",
    ),
    e("source control", "c", "commit the staged files"),
    e("source control", "x", "discard a file's unstaged changes"),
    e("source control", "r  esc", "refresh / back to the sidebar"),
    e(
        "diff view",
        "j k  space u  g G",
        "scroll / page / top-bottom",
    ),
    e("diff view", "esc  q", "close"),
    e(
        "terminal",
        "shift+pgup  shift+pgdn",
        "scroll back while in TERM",
    ),
    e("terminal", "mouse wheel", "scroll back"),
    e(
        "terminal",
        "shift+drag",
        "select text natively (copy with your terminal)",
    ),
    e("quit", "q", "quit — sessions keep running"),
    e(
        "quit",
        "Q",
        "quit and stop the daemon (kills every session)",
    ),
    e(
        "status",
        "⠹ ◐ ✓",
        "agent working / waiting for you / done (unseen)",
    ),
    e(
        "status",
        "○ ▶ ✗ ■",
        "idle / dev server running / failed / ended",
    ),
    e(
        "status",
        "*  ◫  [vscode]",
        "uncommitted changes / VS Code window open",
    ),
];

/// Glossary entries matching every word of `query` (in keys, description or
/// section), with `{unlock}` filled in.
pub fn search(query: &str, unlock: &str) -> Vec<(&'static str, String, &'static str)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    GLOSSARY
        .iter()
        .map(|en| (en.section, en.keys.replace("{unlock}", unlock), en.desc))
        .filter(|(sec, keys, desc)| {
            let hay = format!("{sec} {keys} {desc}").to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect()
}

/// Hints for the status bar, most relevant first; `? keys` always last.
pub fn context_hints(app: &App) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: &str| {
        if !h.iter().any(|(hk, _)| hk == k) {
            h.push((k.to_string(), v.to_string()));
        }
    };
    match (&app.overlay, app.mode) {
        (Some(Overlay::Diff { .. }), _) => {
            push("j/k", "scroll");
            push("space/u", "page");
            push("esc", "close");
            return h;
        }
        (Some(Overlay::Help { .. }), _) => {
            push("type", "to filter");
            push("↑↓", "scroll");
            push("esc", "close");
            return h;
        }
        (Some(_), _) => {
            push("esc", "close");
            return h;
        }
        (None, Mode::Terminal) => {
            push(&app.unlock_label(), "back to NAV");
            push("shift+pgup", "scroll");
            push("shift+drag", "select text");
            return h;
        }
        (None, Mode::Nav) => {}
    }

    if app.git.visible && app.git.focused {
        push("space", "stage/unstage");
        push("enter", "diff");
        push("c", "commit");
        push("a/u", "all");
        push("x", "discard");
        push("esc", "back");
        push("g", "hide");
        push("?", "keys");
        return h;
    }

    let attention = app
        .sessions
        .iter()
        .any(|s| matches!(s.status, Status::Waiting | Status::Done));
    if attention {
        push(".", "next ◐/✓");
    }

    // The active tab decides the most useful keys.
    match app.active_tab() {
        Some(Tab::Group { sessions, .. }) => {
            let alive = sessions
                .iter()
                .filter_map(|id| app.session(id))
                .any(|s| s.alive);
            if alive {
                push("enter", "focus");
                push("s", "stop");
                push("R", "restart");
                if sessions.len() > 1 {
                    push("[ ]", "switch proc");
                }
            } else {
                push("R", "restart");
            }
            push("w", "close");
        }
        Some(Tab::Single(id)) => match app.session(&id) {
            Some(s) if s.alive => {
                push("enter", "focus");
                push("w", "close");
            }
            Some(s) if s.resumable() => {
                push("u", "resume");
                push("w", "close");
            }
            Some(_) => {
                push("u", "restart");
                push("w", "close");
            }
            None => {}
        },
        None => {}
    }
    if app.current_tabs().len() > 1 {
        push("1-9", "tabs");
    }

    match app.selected_row() {
        Some(Row::Project(_)) => {
            push("n", "new worktree");
            push("space", "expand");
        }
        Some(Row::Worktree(..)) | Some(Row::Package(..)) => {
            push("c", "claude");
            push("x", "codex");
            push("t", "shell");
            push("r", "run");
            push("g", "git");
            push("e", "vscode");
            if matches!(app.selected_worktree(), Some((_, w)) if !w.is_main) {
                push("D", "remove wt");
            }
        }
        None => push("a", "add project"),
    }
    push("/", "jump");
    push("q", "quit");
    push("?", "keys");
    h
}

/// Drop hints from the middle until they fit `width` (keeping `? keys`).
pub fn fit(hints: Vec<(String, String)>, width: usize) -> Vec<(String, String)> {
    let cost = |(k, v): &(String, String)| k.chars().count() + v.chars().count() + 3;
    let mut out = hints;
    while out.iter().map(cost).sum::<usize>() > width && out.len() > 1 {
        // Remove the last entry before the trailing `?`.
        let idx = out.len() - 2;
        out.remove(idx);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_words() {
        let r = search("close tab", "ctrl+q");
        assert!(r.iter().any(|(_, k, _)| k == "w"));
        let r = search("leave", "ctrl+o");
        assert_eq!(r[0].1, "ctrl+o");
        assert_eq!(search("", "ctrl+q").len(), GLOSSARY.len());
        assert!(search("stage", "x").len() >= 2);
    }

    #[test]
    fn fit_keeps_help_last() {
        let hints: Vec<(String, String)> = (0..10)
            .map(|i| (format!("k{i}"), "something".into()))
            .chain([("?".into(), "keys".into())])
            .collect();
        let out = fit(hints, 40);
        assert_eq!(out.last().unwrap().0, "?");
        assert!(
            out.iter()
                .map(|(k, v)| k.len() + v.len() + 3)
                .sum::<usize>()
                <= 40
        );
    }
}
