//! Modal overlays: help, confirm, text input, fuzzy picker, and the
//! new-worktree / setup wizard.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hive_core::protocol::*;

pub enum Overlay {
    Help,
    Confirm {
        msg: String,
        action: Action,
    },
    Input {
        title: String,
        input: TextInput,
        action: InputAction,
    },
    Picker(Picker),
    Wizard(Wizard),
    /// Waiting on the daemon (e.g. resolving `choices_cmd`).
    Loading {
        pending: PendingRun,
    },
    /// A scrollable diff (or commit hook output).
    Diff {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
}

impl Overlay {
    pub fn text_input(&mut self) -> Option<&mut TextInput> {
        match self {
            Overlay::Input { input, .. } => Some(input),
            Overlay::Picker(p) => Some(&mut p.filter),
            Overlay::Wizard(w) => w.focused_input(),
            _ => None,
        }
    }
}

pub enum Action {
    CloseSessions(Vec<String>),
    RemoveWorktree {
        project: String,
        path: PathBuf,
        force: bool,
    },
    RemoveProject(String),
    QuitAndStop,
    GitDiscard {
        worktree: PathBuf,
        paths: Vec<String>,
    },
}

pub enum InputAction {
    AddProject,
    Ask(PendingRun),
    Commit { worktree: PathBuf },
}

pub enum PickAction {
    Run { project: String, worktree: PathBuf },
    AskChoice(PendingRun),
    Palette(Vec<PaletteTarget>),
}

pub enum PaletteTarget {
    Worktree(PathBuf),
    Session(String),
}

// ---------------------------------------------------------------- text input

#[derive(Default, Clone)]
pub struct TextInput {
    pub value: String,
    /// Cursor position in chars.
    pub cursor: usize,
}

impl TextInput {
    pub fn set(&mut self, v: &str) {
        self.value = v.to_string();
        self.cursor = v.chars().count();
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.value.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// Returns true if the value changed.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let len = self.value.chars().count();
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                let at = self.byte_at(self.cursor);
                self.value.drain(..at);
                self.cursor = 0;
                return true;
            }
            KeyCode::Char('w') if ctrl => return self.delete_word(),
            KeyCode::Backspace if alt || ctrl => return self.delete_word(),
            KeyCode::Char(c) if !ctrl => {
                self.insert_str(&c.to_string());
                return true;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte_at(self.cursor);
                    self.value.remove(at);
                    return true;
                }
            }
            KeyCode::Delete => {
                if self.cursor < len {
                    let at = self.byte_at(self.cursor);
                    self.value.remove(at);
                    return true;
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => {}
        }
        false
    }

    fn delete_word(&mut self) -> bool {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && chars[i - 1] != ' ' && chars[i - 1] != '/' {
            i -= 1;
        }
        if i == self.cursor && i > 0 {
            i -= 1;
        }
        let (a, b) = (self.byte_at(i), self.byte_at(self.cursor));
        self.value.drain(a..b);
        let changed = i != self.cursor;
        self.cursor = i;
        changed
    }
}

// ---------------------------------------------------------------- fuzzy

/// Subsequence match score (higher is better); `None` if no match.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut ti = 0usize;
    let mut last: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        if qc == ' ' {
            continue;
        }
        let pos = (ti..t.len()).find(|&i| t[i] == qc)?;
        score += 10;
        if last.map(|l| l + 1 == pos).unwrap_or(false) {
            score += 15;
        }
        if pos == 0 || matches!(t[pos - 1], ' ' | '/' | '-' | '_' | '›' | '·' | '.') {
            score += 10;
        }
        score -= (pos - ti) as i64;
        last = Some(pos);
        ti = pos + 1;
    }
    Some(score)
}

pub struct PickItem {
    pub label: String,
    pub detail: String,
}

pub struct Picker {
    pub title: String,
    pub items: Vec<PickItem>,
    pub filter: TextInput,
    /// Index into `filtered()`.
    pub sel: usize,
    pub action: PickAction,
}

impl Picker {
    pub fn new(title: &str, items: Vec<PickItem>, action: PickAction) -> Self {
        Self {
            title: title.into(),
            items,
            filter: TextInput::default(),
            sel: 0,
            action,
        }
    }

    /// Item indices matching the filter, best first.
    pub fn filtered(&self) -> Vec<usize> {
        let q = self.filter.value.trim();
        let mut scored: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                fuzzy_score(q, &format!("{} {}", it.label, it.detail)).map(|s| (s, i))
            })
            .collect();
        if !q.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        scored.into_iter().map(|(_, i)| i).collect()
    }

    pub fn selected(&self) -> Option<usize> {
        self.filtered().get(self.sel).copied()
    }

    pub fn handle_key(&mut self, key: &KeyEvent) {
        let n = self.filtered().len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Down | KeyCode::Tab => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::BackTab => self.sel = self.sel.saturating_sub(1),
            KeyCode::Char('p') | KeyCode::Char('k') if ctrl => {
                self.sel = self.sel.saturating_sub(1)
            }
            _ => {
                if self.filter.handle_key(key) {
                    self.sel = 0;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- pending run

pub struct PendingRun {
    pub project: String,
    pub worktree: PathBuf,
    pub target: String,
    pub asks: Vec<AskInfo>,
    pub idx: usize,
    pub answers: BTreeMap<String, String>,
}

impl PendingRun {
    pub fn new(project: String, worktree: PathBuf, run: &RunInfo) -> Self {
        Self {
            project,
            worktree,
            target: run.name.clone(),
            asks: run.asks.clone(),
            idx: 0,
            answers: BTreeMap::new(),
        }
    }

    pub fn current(&self) -> Option<&AskInfo> {
        self.asks.get(self.idx)
    }

    pub fn answer(&mut self, value: String) {
        if let Some(a) = self.asks.get(self.idx) {
            self.answers.insert(a.name.clone(), value);
            self.idx += 1;
        }
    }
}

// ---------------------------------------------------------------- wizard

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Mode,
    Branch,
    Base,
    Env,
    Step(usize),
}

pub enum WizardResult {
    Create(CreateWorktree),
    Setup {
        project: String,
        worktree: PathBuf,
        steps: Vec<String>,
        env: Option<String>,
    },
}

pub struct Wizard {
    pub project: String,
    pub project_name: String,
    /// `Some` = re-run setup in this existing worktree.
    pub setup_only: Option<(PathBuf, String)>,
    pub mode: WorktreeMode,
    pub branch: TextInput,
    pub base: TextInput,
    pub focus: usize,
    pub env_choices: Vec<String>,
    pub env_idx: usize,
    pub steps: Vec<(StepInfo, bool)>,
    pub steps_touched: bool,
    pub profiles: Vec<ProfileInfo>,
    pub profile: Option<String>,
    pub local: Vec<String>,
    pub remote: Vec<String>,
    pub loading_branches: bool,
    pub sugg_sel: Option<usize>,
    pub error: Option<String>,
    pub worktree_root: PathBuf,
}

pub const MODES: [WorktreeMode; 4] = [
    WorktreeMode::NewBranch,
    WorktreeMode::Local,
    WorktreeMode::Remote,
    WorktreeMode::Detached,
];

pub fn mode_label(m: WorktreeMode) -> &'static str {
    match m {
        WorktreeMode::NewBranch => "new branch",
        WorktreeMode::Local => "existing local",
        WorktreeMode::Remote => "from remote",
        WorktreeMode::Detached => "detached",
    }
}

impl Wizard {
    fn base(p: &ProjectInfo) -> Self {
        let mut base = TextInput::default();
        base.set(&p.base_branch);
        let has_env = p.steps.iter().any(|_| true) && !p.env_choices.is_empty();
        Self {
            project: p.id.clone(),
            project_name: p.name.clone(),
            setup_only: None,
            mode: WorktreeMode::NewBranch,
            branch: TextInput::default(),
            base,
            focus: 0,
            env_choices: if has_env {
                p.env_choices.clone()
            } else {
                vec![]
            },
            env_idx: 0,
            steps: p
                .steps
                .iter()
                .map(|s| (s.clone(), p.profiles.is_empty()))
                .collect(),
            steps_touched: false,
            profiles: p.profiles.clone(),
            profile: None,
            local: vec![],
            remote: vec![],
            loading_branches: true,
            sugg_sel: None,
            error: None,
            worktree_root: p.worktree_root.clone(),
        }
    }

    pub fn create(p: &ProjectInfo) -> Self {
        let mut w = Self::base(p);
        w.apply_profile();
        w
    }

    pub fn setup(p: &ProjectInfo, wt: &WorktreeInfo) -> Self {
        let mut w = Self::base(p);
        w.setup_only = Some((wt.path.clone(), wt.label()));
        w.loading_branches = false;
        w.branch.set(wt.branch.as_deref().unwrap_or(""));
        w.apply_profile();
        w
    }

    pub fn fields(&self) -> Vec<Field> {
        let mut f = Vec::new();
        if self.setup_only.is_none() {
            f.push(Field::Mode);
            f.push(Field::Branch);
            if self.mode == WorktreeMode::NewBranch {
                f.push(Field::Base);
            }
        }
        if !self.env_choices.is_empty() {
            f.push(Field::Env);
        }
        f.extend((0..self.steps.len()).map(Field::Step));
        f
    }

    pub fn focused(&self) -> Field {
        let f = self.fields();
        f[self.focus.min(f.len().saturating_sub(1))]
    }

    pub fn focused_input(&mut self) -> Option<&mut TextInput> {
        match self.focused() {
            Field::Branch => Some(&mut self.branch),
            Field::Base => Some(&mut self.base),
            _ => None,
        }
    }

    /// Branch suggestions for local/remote modes (or base suggestions).
    pub fn suggestions(&self) -> Vec<String> {
        let (pool, query): (&[String], &str) = match (self.focused(), self.mode) {
            (Field::Branch, WorktreeMode::Local) | (Field::Branch, WorktreeMode::Detached) => {
                (&self.local, &self.branch.value)
            }
            (Field::Branch, WorktreeMode::Remote) => (&self.remote, &self.branch.value),
            (Field::Base, _) => (&self.local, &self.base.value),
            _ => return vec![],
        };
        let mut scored: Vec<(i64, &String)> = pool
            .iter()
            .filter_map(|b| fuzzy_score(query, b).map(|s| (s, b)))
            .collect();
        if !query.is_empty() {
            scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        }
        scored.into_iter().take(8).map(|(_, b)| b.clone()).collect()
    }

    pub fn enter_selects_suggestion(&self) -> bool {
        self.sugg_sel.is_some() && !self.suggestions().is_empty()
    }

    pub fn on_branch_changed(&mut self) {
        self.sugg_sel = None;
        self.apply_profile();
    }

    fn apply_profile(&mut self) {
        let b = self.branch.value.trim();
        let b = b.strip_prefix("origin/").unwrap_or(b);
        let prof = self
            .profiles
            .iter()
            .find(|p| {
                p.patterns
                    .iter()
                    .any(|pat| hive_core::sanitize::match_pattern(b, pat))
            })
            .or_else(|| self.profiles.last());
        self.profile = prof.map(|p| p.name.clone());
        if let Some(p) = prof {
            if !self.steps_touched {
                for (s, on) in self.steps.iter_mut() {
                    *on = p.steps.contains(&s.id);
                }
            }
            if let Some(env) = &p.env {
                if let Some(i) = self.env_choices.iter().position(|e| e == env) {
                    self.env_idx = i;
                }
            }
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) {
        self.error = None;
        let n = self.fields().len();
        let field = self.focused();
        let sugg = self.suggestions();
        match key.code {
            KeyCode::Enter => {
                // Accept the highlighted suggestion.
                if let Some(i) = self.sugg_sel {
                    if let Some(s) = sugg.get(i) {
                        let s = s.clone();
                        match field {
                            Field::Base => self.base.set(&s),
                            _ => self.branch.set(&s),
                        }
                        self.sugg_sel = None;
                        self.apply_profile();
                    }
                }
            }
            KeyCode::Tab => self.focus = (self.focus + 1) % n,
            KeyCode::BackTab => self.focus = (self.focus + n - 1) % n,
            KeyCode::Down if !sugg.is_empty() && matches!(field, Field::Branch | Field::Base) => {
                let next = self.sugg_sel.map(|i| i + 1).unwrap_or(0);
                if next >= sugg.len() {
                    self.sugg_sel = None;
                    self.focus = (self.focus + 1) % n;
                } else {
                    self.sugg_sel = Some(next);
                }
            }
            KeyCode::Up if self.sugg_sel.is_some() => {
                self.sugg_sel = self.sugg_sel.and_then(|i| i.checked_sub(1));
            }
            KeyCode::Down => {
                self.sugg_sel = None;
                self.focus = (self.focus + 1) % n;
            }
            KeyCode::Up => {
                self.sugg_sel = None;
                self.focus = (self.focus + n - 1) % n;
            }
            KeyCode::Left | KeyCode::Right if matches!(field, Field::Mode | Field::Env) => {
                let d: isize = if key.code == KeyCode::Right { 1 } else { -1 };
                match field {
                    Field::Mode => {
                        let i = MODES.iter().position(|m| *m == self.mode).unwrap() as isize;
                        self.mode = MODES[((i + d).rem_euclid(MODES.len() as isize)) as usize];
                        self.branch.set("");
                        self.sugg_sel = None;
                    }
                    Field::Env => {
                        let len = self.env_choices.len() as isize;
                        self.env_idx = ((self.env_idx as isize + d).rem_euclid(len)) as usize;
                    }
                    _ => {}
                }
            }
            KeyCode::Char(' ') if matches!(field, Field::Step(_)) => {
                if let Field::Step(i) = field {
                    self.steps[i].1 = !self.steps[i].1;
                    self.steps_touched = true;
                }
            }
            _ => {
                let changed = match field {
                    Field::Branch => self.branch.handle_key(key),
                    Field::Base => self.base.handle_key(key),
                    _ => false,
                };
                if changed {
                    if field == Field::Branch {
                        self.on_branch_changed();
                    } else {
                        self.sugg_sel = None;
                    }
                }
            }
        }
    }

    pub fn env(&self) -> Option<String> {
        self.env_choices.get(self.env_idx).cloned()
    }

    pub fn dest_preview(&self) -> String {
        let b = self.branch.value.trim();
        let b = match self.mode {
            WorktreeMode::Remote => b.split_once('/').map(|(_, r)| r).unwrap_or(b),
            _ => b,
        };
        hive_core::paths::tildify(
            &self
                .worktree_root
                .join(hive_core::sanitize::sanitize_branch_name(b)),
        )
    }

    pub fn submit(&self) -> Result<WizardResult, String> {
        let steps: Vec<String> = self
            .steps
            .iter()
            .filter(|(_, on)| *on)
            .map(|(s, _)| s.id.clone())
            .collect();
        if let Some((path, _)) = &self.setup_only {
            if steps.is_empty() {
                return Err("select at least one step".into());
            }
            return Ok(WizardResult::Setup {
                project: self.project.clone(),
                worktree: path.clone(),
                steps,
                env: self.env(),
            });
        }
        let branch = self.branch.value.trim().to_string();
        if branch.is_empty() {
            return Err("branch is required".into());
        }
        if self.mode == WorktreeMode::NewBranch && branch.contains(' ') {
            return Err("branch names can't contain spaces".into());
        }
        Ok(WizardResult::Create(CreateWorktree {
            project: self.project.clone(),
            mode: self.mode,
            branch,
            base: Some(self.base.value.trim().to_string()).filter(|b| !b.is_empty()),
            steps,
            env: self.env(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("fl", "feature/login").is_some());
        assert!(fuzzy_score("xyz", "feature/login").is_none());
        assert!(
            fuzzy_score("login", "feature/login").unwrap()
                > fuzzy_score("login", "l-o-g-i-n").unwrap()
        );
    }

    #[test]
    fn text_input_editing() {
        let mut t = TextInput::default();
        t.set("feature/lögin");
        t.handle_key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(t.value, "feature/lögi");
        t.handle_key(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.value, "feature/");
        t.handle_key(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        t.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(t.value, "xfeature/");
    }

    fn project() -> ProjectInfo {
        ProjectInfo {
            id: "p".into(),
            name: "p".into(),
            root: "/r".into(),
            worktree_root: "/wt".into(),
            base_branch: "develop".into(),
            config_source: None,
            config_error: None,
            worktrees: vec![],
            packages: vec![],
            runs: vec![],
            steps: vec![
                StepInfo {
                    id: "install".into(),
                    label: "yarn install".into(),
                },
                StepInfo {
                    id: "ios".into(),
                    label: "yarn ios".into(),
                },
            ],
            profiles: vec![
                ProfileInfo {
                    name: "native".into(),
                    patterns: vec!["native/*".into()],
                    env: Some("prod".into()),
                    steps: vec!["install".into(), "ios".into()],
                },
                ProfileInfo {
                    name: "default".into(),
                    patterns: vec!["*".into()],
                    env: Some("staging".into()),
                    steps: vec!["install".into()],
                },
            ],
            env_choices: vec!["staging".into(), "prod".into()],
            port_stride: 10,
        }
    }

    #[test]
    fn wizard_profile_prechecks() {
        let mut w = Wizard::create(&project());
        w.focus = 1;
        for c in "native/cam".chars() {
            w.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(w.profile.as_deref(), Some("native"));
        assert!(w.steps.iter().all(|(_, on)| *on));
        assert_eq!(w.env().as_deref(), Some("prod"));
        assert_eq!(w.dest_preview(), "/wt/native-cam");
        match w.submit().unwrap() {
            WizardResult::Create(c) => {
                assert_eq!(c.steps, vec!["install", "ios"]);
                assert_eq!(c.base.as_deref(), Some("develop"));
            }
            _ => panic!(),
        }
    }
}
