//! The source-control panel: the selected worktree's changes, split like
//! VS Code into Staged and Changes, with stage / unstage / discard / diff /
//! commit. Only staged files are committed, so local tweaks can stay put.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hive_core::protocol::{ClientRequest, GitFile, GitStatusInfo, ToastLevel};
use ratatui::layout::Rect;

use crate::app::App;
use crate::overlay::{Action, InputAction, Overlay, TextInput};

/// How often the visible panel re-reads `git status`.
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Staged,
    Changes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRow {
    Header(Section),
    File(Section, usize),
}

pub struct GitPanel {
    pub visible: bool,
    pub focused: bool,
    pub status: Option<GitStatusInfo>,
    /// Index into `rows()`.
    pub sel: usize,
    /// Scroll offset of the list.
    pub offset: usize,
    pub rect: Rect,
    /// List area inside the panel (for mouse hits).
    pub list: Rect,
    in_flight: bool,
    last_request: Instant,
}

impl Default for GitPanel {
    fn default() -> Self {
        Self {
            visible: false,
            focused: false,
            status: None,
            sel: 0,
            offset: 0,
            rect: Rect::default(),
            list: Rect::default(),
            in_flight: false,
            last_request: Instant::now() - REFRESH * 2,
        }
    }
}

impl GitPanel {
    pub fn files(&self, section: Section) -> Vec<usize> {
        let Some(st) = &self.status else {
            return vec![];
        };
        st.files
            .iter()
            .enumerate()
            .filter(|(_, f)| match section {
                Section::Staged => f.staged.is_some(),
                Section::Changes => f.unstaged.is_some(),
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Headers are shown only for non-empty sections ("Changes" always, so a
    /// clean tree still has a row to sit on).
    pub fn rows(&self) -> Vec<GitRow> {
        let mut rows = Vec::new();
        let staged = self.files(Section::Staged);
        if !staged.is_empty() {
            rows.push(GitRow::Header(Section::Staged));
            rows.extend(staged.into_iter().map(|i| GitRow::File(Section::Staged, i)));
        }
        rows.push(GitRow::Header(Section::Changes));
        rows.extend(
            self.files(Section::Changes)
                .into_iter()
                .map(|i| GitRow::File(Section::Changes, i)),
        );
        rows
    }

    pub fn file(&self, i: usize) -> Option<&GitFile> {
        self.status.as_ref()?.files.get(i)
    }

    pub fn selected(&self) -> Option<GitRow> {
        let rows = self.rows();
        rows.get(self.sel.min(rows.len().saturating_sub(1)))
            .copied()
    }

    fn worktree(&self) -> Option<PathBuf> {
        self.status.as_ref().map(|s| s.worktree.clone())
    }

    /// Keep the selection on the same file across refreshes when possible.
    fn apply_status(&mut self, info: GitStatusInfo) {
        let prev = self.selected().and_then(|r| match r {
            GitRow::File(sec, i) => self.file(i).map(|f| (sec, f.path.clone())),
            GitRow::Header(sec) => Some((sec, String::new())),
        });
        let same_tree = self.worktree().as_ref() == Some(&info.worktree);
        self.status = Some(info);
        if !same_tree {
            self.sel = 0;
            self.offset = 0;
        }
        if let Some((sec, path)) = prev.filter(|_| same_tree) {
            let rows = self.rows();
            let found = rows.iter().position(|r| match *r {
                GitRow::File(s, i) => {
                    s == sec && self.file(i).map(|f| f.path == path).unwrap_or(false)
                }
                GitRow::Header(s) => s == sec && path.is_empty(),
            });
            if let Some(i) = found {
                self.sel = i;
            }
        }
        self.sel = self.sel.min(self.rows().len().saturating_sub(1));
    }
}

impl App {
    /// Ask for `git status` when the panel is visible and its data is stale
    /// or belongs to another worktree.
    pub fn git_tick(&mut self, force: bool) {
        if !self.git.visible {
            return;
        }
        let Some((_, wt)) = self.selected_worktree() else {
            return;
        };
        let wt = wt.path.clone();
        let other_tree = self.git.worktree().as_ref() != Some(&wt);
        if other_tree {
            // Don't show another worktree's changes while loading.
            self.git.status = None;
            self.git.in_flight = false;
        }
        let stale = self.git.last_request.elapsed() >= REFRESH;
        if force || other_tree || (stale && !self.git.in_flight) {
            self.git.in_flight = true;
            self.git.last_request = Instant::now();
            self.send(ClientRequest::GitStatus { worktree: wt });
        }
    }

    pub fn git_on_status(&mut self, info: GitStatusInfo) {
        let current = self.selected_worktree().map(|(_, w)| w.path.clone());
        if current.as_ref() != Some(&info.worktree) {
            return;
        }
        self.git.in_flight = false;
        self.git.apply_status(info);
        self.dirty = true;
    }

    pub fn git_toggle(&mut self) {
        if self.git.visible && self.git.focused {
            self.git.visible = false;
            self.git.focused = false;
        } else {
            self.git.visible = true;
            self.git.focused = true;
            self.git_tick(true);
        }
        self.ui_dirty = true;
        self.dirty = true;
    }

    fn git_paths(&self, row: GitRow) -> (Section, Vec<String>) {
        match row {
            GitRow::File(sec, i) => (
                sec,
                self.git
                    .file(i)
                    .map(|f| vec![f.path.clone()])
                    .unwrap_or_default(),
            ),
            GitRow::Header(sec) => (
                sec,
                self.git
                    .files(sec)
                    .into_iter()
                    .filter_map(|i| self.git.file(i).map(|f| f.path.clone()))
                    .collect(),
            ),
        }
    }

    fn git_stage(&mut self, paths: Vec<String>) {
        if let (Some(wt), false) = (self.git.worktree(), paths.is_empty()) {
            self.send(ClientRequest::GitStage {
                worktree: wt,
                paths,
            });
        }
    }

    fn git_unstage(&mut self, paths: Vec<String>) {
        if let (Some(wt), false) = (self.git.worktree(), paths.is_empty()) {
            self.send(ClientRequest::GitUnstage {
                worktree: wt,
                paths,
            });
        }
    }

    fn git_open_diff(&mut self) {
        let (Some(GitRow::File(sec, i)), Some(wt)) = (self.git.selected(), self.git.worktree())
        else {
            return;
        };
        let Some(f) = self.git.file(i) else { return };
        self.send(ClientRequest::GitDiff {
            worktree: wt,
            path: f.path.clone(),
            staged: sec == Section::Staged,
        });
    }

    fn git_commit_prompt(&mut self) {
        let Some(wt) = self.git.worktree() else {
            return;
        };
        let n = self.git.files(Section::Staged).len();
        if n == 0 {
            return self.toast(
                ToastLevel::Warn,
                "nothing staged — press space on a file (or a header) to stage it",
            );
        }
        self.overlay = Some(Overlay::Input {
            title: format!("Commit {n} staged file{}", if n == 1 { "" } else { "s" }),
            input: TextInput::default(),
            action: InputAction::Commit { worktree: wt },
        });
    }

    fn git_discard_prompt(&mut self) {
        let Some(row) = self.git.selected() else {
            return;
        };
        let (sec, paths) = self.git_paths(row);
        let Some(wt) = self.git.worktree() else {
            return;
        };
        if sec != Section::Changes || paths.is_empty() {
            return self.toast(
                ToastLevel::Info,
                "discard works on unstaged changes — unstage first",
            );
        }
        let what = if paths.len() == 1 {
            format!("“{}”", paths[0])
        } else {
            format!("{} files", paths.len())
        };
        self.overlay = Some(Overlay::Confirm {
            msg: format!("Discard unstaged changes to {what}?\nUntracked files are deleted. This can't be undone."),
            action: Action::GitDiscard { worktree: wt, paths },
        });
    }

    /// Keys while the panel has focus. Returns false to let NAV handle it.
    pub fn git_key(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let n = self.git.rows().len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.git.sel = (self.git.sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => self.git.sel = self.git.sel.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => {
                if key.code == KeyCode::Char('g') {
                    self.git_toggle();
                } else {
                    self.git.sel = 0;
                }
            }
            KeyCode::End | KeyCode::Char('G') => self.git.sel = n.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(row) = self.git.selected() {
                    match self.git_paths(row) {
                        (Section::Changes, p) => self.git_stage(p),
                        (Section::Staged, p) => self.git_unstage(p),
                    }
                }
            }
            KeyCode::Char('a') => {
                let p = self.git_paths(GitRow::Header(Section::Changes)).1;
                self.git_stage(p);
            }
            KeyCode::Char('u') => {
                let p = self.git_paths(GitRow::Header(Section::Staged)).1;
                self.git_unstage(p);
            }
            KeyCode::Char('x') => self.git_discard_prompt(),
            KeyCode::Enter | KeyCode::Char('d') => self.git_open_diff(),
            KeyCode::Char('c') => self.git_commit_prompt(),
            KeyCode::Char('r') => self.git_tick(true),
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left | KeyCode::Tab => {
                self.git.focused = false
            }
            KeyCode::Char('?') => self.overlay = Some(Overlay::help()),
            _ => return false,
        }
        self.dirty = true;
        true
    }

    /// Click/scroll inside the panel. Returns true if handled.
    pub fn git_mouse(
        &mut self,
        kind: crossterm::event::MouseEventKind,
        col: u16,
        row: u16,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        let pos = ratatui::layout::Position { x: col, y: row };
        if !self.git.visible || !self.git.rect.contains(pos) {
            return false;
        }
        // Clicking the panel takes keyboard focus from the terminal too.
        self.git.focused = true;
        self.mode = crate::app::Mode::Nav;
        let n = self.git.rows().len();
        match kind {
            MouseEventKind::ScrollUp => self.git.sel = self.git.sel.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                self.git.sel = (self.git.sel + 1).min(n.saturating_sub(1))
            }
            MouseEventKind::Down(MouseButton::Left) if self.git.list.contains(pos) => {
                let i = self.git.offset + (row - self.git.list.y) as usize;
                if i < n {
                    if self.git.sel == i {
                        self.git_open_diff();
                    }
                    self.git.sel = i;
                }
            }
            _ => {}
        }
        self.dirty = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, staged: Option<char>, unstaged: Option<char>) -> GitFile {
        GitFile {
            path: path.into(),
            orig_path: None,
            staged,
            unstaged,
        }
    }

    fn panel(files: Vec<GitFile>) -> GitPanel {
        let mut p = GitPanel::default();
        p.apply_status(GitStatusInfo {
            worktree: "/w".into(),
            files,
            ..Default::default()
        });
        p
    }

    #[test]
    fn sections() {
        let p = panel(vec![
            file("a", Some('M'), None),
            file("b", Some('M'), Some('M')),
            file("c", None, Some('?')),
        ]);
        let rows = p.rows();
        assert_eq!(rows[0], GitRow::Header(Section::Staged));
        assert_eq!(p.files(Section::Staged), vec![0, 1]);
        assert_eq!(p.files(Section::Changes), vec![1, 2]);
        assert_eq!(rows.len(), 2 + 1 + 2 + 1);
        let clean = panel(vec![]);
        assert_eq!(clean.rows(), vec![GitRow::Header(Section::Changes)]);
    }

    #[test]
    fn selection_follows_file() {
        let mut p = panel(vec![file("a", None, Some('M')), file("b", None, Some('M'))]);
        p.sel = 2; // Changes: b
        p.apply_status(GitStatusInfo {
            worktree: "/w".into(),
            files: vec![file("a", Some('M'), None), file("b", None, Some('M'))],
            ..Default::default()
        });
        assert_eq!(p.selected(), Some(GitRow::File(Section::Changes, 1)));
    }
}
