//! TUI state and everything that changes it. Rendering lives in `ui`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use hive_core::config::GlobalConfig;
use hive_core::protocol::*;
use ratatui::layout::{Position, Rect};
use serde::{Deserialize, Serialize};

use crate::client::ReqTx;
use crate::keys::{
    encode_key, encode_mouse, matches_binding, matches_exact, nav_remap, parse_binding,
};
use crate::overlay::*;
use crate::term::AttachedTerm;

/// Recently viewed terminals kept parsed, so switching back is a delta.
const TERM_CACHE_MAX: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Nav,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Project(usize),
    Worktree(usize, usize),
    Package(usize, usize, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tab {
    Single(String),
    Group {
        target: String,
        sessions: Vec<String>,
    },
}

impl Tab {
    pub fn key(&self) -> String {
        match self {
            Tab::Single(id) => id.clone(),
            Tab::Group { target, .. } => format!("run:{target}"),
        }
    }

    pub fn sessions(&self) -> Vec<String> {
        match self {
            Tab::Single(id) => vec![id.clone()],
            Tab::Group { sessions, .. } => sessions.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Areas {
    pub sidebar: Rect,
    pub tree: Rect,
    pub header: Rect,
    pub tabs: Rect,
    pub pane: Rect,
    pub status: Rect,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UiState {
    expanded: Vec<String>,
    selected: Option<String>,
    active_tabs: HashMap<String, String>,
    sidebar_width: Option<u16>,
    #[serde(default)]
    vscode: Vec<String>,
}

pub struct App {
    pub tx: ReqTx,
    pub global: GlobalConfig,
    unlock: (KeyModifiers, KeyCode),
    nav_remap: crate::keys::Remap,
    /// Dragging the sidebar border.
    resizing: bool,
    pub projects: Vec<ProjectInfo>,
    pub sessions: Vec<SessionInfo>,
    pub expanded: HashSet<String>,
    pub rows: Vec<Row>,
    pub sel: usize,
    pub active_tab: HashMap<PathBuf, String>,
    pub group_focus: HashMap<String, String>,
    pub terms: HashMap<String, AttachedTerm>,
    term_lru: VecDeque<String>,
    attached: HashSet<String>,
    pub mode: Mode,
    pub overlay: Option<Overlay>,
    pub toast: Option<(ToastLevel, String, Instant)>,
    pub sidebar_width: u16,
    /// Worktrees whose VS Code window hive has opened (the `[vscode]` badge).
    pub vscode_opened: HashSet<PathBuf>,
    pub areas: Areas,
    /// Pane rects of the visible sessions (one, or one per run proc).
    pub pane_rects: Vec<(String, Rect)>,
    pub tab_hits: Vec<(Rect, String)>,
    pub dirty: bool,
    pub quit: bool,
    pub tick: u64,
    pub connected: bool,
    seen_status: HashMap<String, Status>,
    ui_dirty: bool,
    last_ui_save: Instant,
    pending_focus: Option<String>,
    snapshot_received: bool,
}

impl App {
    pub fn new(tx: ReqTx, global: GlobalConfig) -> Self {
        let unlock = parse_binding(&global.keys.unlock)
            .unwrap_or((KeyModifiers::CONTROL, KeyCode::Char('q')));
        let sidebar_width = global.ui.sidebar_width;
        let (nav_remap, key_errors) = nav_remap(&global.keys.nav);
        let toast = (!key_errors.is_empty())
            .then(|| (ToastLevel::Error, key_errors.join("; "), Instant::now()));
        Self {
            tx,
            global,
            unlock,
            nav_remap,
            resizing: false,
            projects: vec![],
            sessions: vec![],
            expanded: HashSet::new(),
            rows: vec![],
            sel: 0,
            active_tab: HashMap::new(),
            group_focus: HashMap::new(),
            terms: HashMap::new(),
            term_lru: VecDeque::new(),
            attached: HashSet::new(),
            mode: Mode::Nav,
            overlay: None,
            toast,
            sidebar_width,
            vscode_opened: HashSet::new(),
            areas: Areas::default(),
            pane_rects: vec![],
            tab_hits: vec![],
            dirty: true,
            quit: false,
            tick: 0,
            connected: true,
            seen_status: HashMap::new(),
            ui_dirty: false,
            last_ui_save: Instant::now(),
            pending_focus: None,
            snapshot_received: false,
        }
    }

    pub fn send(&self, req: ClientRequest) {
        let _ = self.tx.send(req);
    }

    pub fn toast(&mut self, level: ToastLevel, msg: impl Into<String>) {
        self.toast = Some((level, msg.into(), Instant::now()));
        self.dirty = true;
    }

    pub fn unlock_label(&self) -> String {
        self.global.keys.unlock.clone()
    }

    // ------------------------------------------------------------ selection

    pub fn rebuild_rows(&mut self) {
        let prev = self.rows.get(self.sel).map(|r| self.row_key(r));
        let mut rows = Vec::new();
        for (pi, p) in self.projects.iter().enumerate() {
            rows.push(Row::Project(pi));
            if !self.expanded.contains(&format!("p:{}", p.id)) {
                continue;
            }
            for (wi, w) in p.worktrees.iter().enumerate() {
                rows.push(Row::Worktree(pi, wi));
                if self.expanded.contains(&format!("w:{}", w.path.display())) {
                    for (ki, k) in p.packages.iter().enumerate() {
                        if w.path.join(&k.path).exists() {
                            rows.push(Row::Package(pi, wi, ki));
                        }
                    }
                }
            }
        }
        self.rows = rows;
        if let Some(k) = prev {
            if let Some(i) = self.rows.iter().position(|r| self.row_key(r) == k) {
                self.sel = i;
            }
        }
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }

    pub fn row_key(&self, r: &Row) -> String {
        match *r {
            Row::Project(pi) => format!("p:{}", self.projects[pi].id),
            Row::Worktree(pi, wi) => {
                format!("w:{}", self.projects[pi].worktrees[wi].path.display())
            }
            Row::Package(pi, wi, ki) => {
                format!(
                    "k:{}:{}",
                    self.projects[pi].worktrees[wi].path.display(),
                    self.projects[pi].packages[ki].name
                )
            }
        }
    }

    pub fn selected_row(&self) -> Option<Row> {
        self.rows.get(self.sel).copied()
    }

    pub fn selected_project(&self) -> Option<&ProjectInfo> {
        let r = self.selected_row()?;
        let pi = match r {
            Row::Project(pi) | Row::Worktree(pi, _) | Row::Package(pi, _, _) => pi,
        };
        self.projects.get(pi)
    }

    /// The worktree the selection is scoped to (the main one for a project row).
    pub fn selected_worktree(&self) -> Option<(&ProjectInfo, &WorktreeInfo)> {
        let r = self.selected_row()?;
        match r {
            Row::Project(pi) => {
                let p = self.projects.get(pi)?;
                Some((
                    p,
                    p.worktrees
                        .iter()
                        .find(|w| w.is_main)
                        .or(p.worktrees.first())?,
                ))
            }
            Row::Worktree(pi, wi) | Row::Package(pi, wi, _) => {
                let p = self.projects.get(pi)?;
                Some((p, p.worktrees.get(wi)?))
            }
        }
    }

    /// Working directory for new sessions: the package if one is selected.
    fn scope_dir(&self) -> Option<PathBuf> {
        match self.selected_row()? {
            Row::Package(pi, wi, ki) => {
                let p = &self.projects[pi];
                Some(p.worktrees[wi].path.join(&p.packages[ki].path))
            }
            _ => self.selected_worktree().map(|(_, w)| w.path.clone()),
        }
    }

    fn select_key(&mut self, key: &str) {
        if let Some(i) = self.rows.iter().position(|r| self.row_key(r) == key) {
            self.sel = i;
        }
    }

    /// Expand a worktree's project and select its row.
    fn reveal_worktree(&mut self, path: &Path) {
        if let Some(p) = self
            .projects
            .iter()
            .find(|p| p.worktrees.iter().any(|w| w.path == path))
        {
            self.expanded.insert(format!("p:{}", p.id));
        }
        self.rebuild_rows();
        let key = format!("w:{}", path.display());
        let already = matches!(self.selected_worktree(), Some((_, w)) if w.path == path);
        if !already {
            self.select_key(&key);
        }
        self.ui_dirty = true;
    }

    // ------------------------------------------------------------ tabs

    pub fn tabs_for(&self, worktree: &Path) -> Vec<Tab> {
        let mut tabs: Vec<Tab> = Vec::new();
        for s in self.sessions.iter().filter(|s| s.worktree == worktree) {
            match s.group() {
                Some(target) => {
                    if let Some(Tab::Group { sessions, .. }) = tabs
                        .iter_mut()
                        .find(|t| matches!(t, Tab::Group { target: t2, .. } if t2 == target))
                    {
                        sessions.push(s.id.clone());
                    } else {
                        tabs.push(Tab::Group {
                            target: target.to_string(),
                            sessions: vec![s.id.clone()],
                        });
                    }
                }
                None => tabs.push(Tab::Single(s.id.clone())),
            }
        }
        tabs
    }

    pub fn current_tabs(&self) -> Vec<Tab> {
        self.selected_worktree()
            .map(|(_, w)| self.tabs_for(&w.path))
            .unwrap_or_default()
    }

    pub fn active_tab(&self) -> Option<Tab> {
        let (_, w) = self.selected_worktree()?;
        let tabs = self.tabs_for(&w.path);
        let key = self.active_tab.get(&w.path);
        key.and_then(|k| tabs.iter().find(|t| &t.key() == k).cloned())
            .or_else(|| tabs.first().cloned())
    }

    fn group_key(&self, worktree: &Path, target: &str) -> String {
        format!("{}#{target}", worktree.display())
    }

    /// The session keys go to: the single one, or the focused proc of a group.
    pub fn focused_session(&self) -> Option<String> {
        let tab = self.active_tab()?;
        match tab {
            Tab::Single(id) => Some(id),
            Tab::Group { target, sessions } => {
                let (_, w) = self.selected_worktree()?;
                let gk = self.group_key(&w.path, &target);
                self.group_focus
                    .get(&gk)
                    .filter(|id| sessions.contains(id))
                    .cloned()
                    .or_else(|| sessions.first().cloned())
            }
        }
    }

    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn set_active(&mut self, worktree: PathBuf, key: String) {
        self.active_tab.insert(worktree, key);
        self.ui_dirty = true;
        self.dirty = true;
    }

    fn focus_session(&mut self, id: &str) {
        let Some(s) = self.session(id).cloned() else {
            self.pending_focus = Some(id.to_string());
            return;
        };
        self.reveal_worktree(&s.worktree);
        let key = match s.group() {
            Some(t) => {
                let gk = self.group_key(&s.worktree, t);
                self.group_focus.insert(gk, s.id.clone());
                format!("run:{t}")
            }
            None => s.id.clone(),
        };
        self.set_active(s.worktree.clone(), key);
    }

    fn cycle_tab(&mut self, delta: isize) {
        let Some((_, w)) = self.selected_worktree() else {
            return;
        };
        let path = w.path.clone();
        let tabs = self.tabs_for(&path);
        if tabs.is_empty() {
            return;
        }
        let cur = self.active_tab().map(|t| t.key());
        let i = cur
            .and_then(|k| tabs.iter().position(|t| t.key() == k))
            .unwrap_or(0) as isize;
        let n = tabs.len() as isize;
        let next = ((i + delta) % n + n) % n;
        self.set_active(path, tabs[next as usize].key());
    }

    fn cycle_group_focus(&mut self, delta: isize) {
        let Some(Tab::Group { target, sessions }) = self.active_tab() else {
            return;
        };
        let Some((_, w)) = self.selected_worktree() else {
            return;
        };
        let gk = self.group_key(&w.path, &target);
        let cur = self
            .focused_session()
            .and_then(|id| sessions.iter().position(|s| *s == id))
            .unwrap_or(0) as isize;
        let n = sessions.len() as isize;
        let next = ((cur + delta) % n + n) % n;
        self.group_focus.insert(gk, sessions[next as usize].clone());
        self.dirty = true;
    }

    // ------------------------------------------------------------ server events

    pub fn on_server(&mut self, ev: ServerEvent) {
        self.dirty = true;
        match ev {
            ServerEvent::Hello { version, .. } => {
                if version != hive_core::PROTOCOL_VERSION {
                    self.toast(
                        ToastLevel::Error,
                        format!("daemon speaks protocol v{version}, this TUI v{} — run `hive daemon restart`", hive_core::PROTOCOL_VERSION),
                    );
                }
            }
            ServerEvent::Snapshot {
                projects,
                sessions,
                ui_state,
            } => {
                self.projects = projects;
                for s in &sessions {
                    self.seen_status.insert(s.id.clone(), s.status);
                }
                self.sessions = sessions;
                if !self.snapshot_received {
                    self.snapshot_received = true;
                    self.restore_ui(ui_state.as_deref());
                }
                self.rebuild_rows();
            }
            ServerEvent::Projects(projects) => {
                self.projects = projects;
                self.rebuild_rows();
            }
            ServerEvent::SessionUpsert(info) => {
                self.notify_transition(&info);
                let id = info.id.clone();
                match self.sessions.iter_mut().find(|s| s.id == id) {
                    Some(s) => *s = info,
                    None => self.sessions.push(info),
                }
                if self.pending_focus.as_deref() == Some(id.as_str()) {
                    self.pending_focus = None;
                    self.focus_session(&id);
                }
            }
            ServerEvent::SessionRemoved { session } => {
                self.sessions.retain(|s| s.id != session);
                self.terms.remove(&session);
                self.attached.remove(&session);
                self.term_lru.retain(|s| *s != session);
            }
            ServerEvent::Scrollback {
                session,
                base_seq,
                data,
                kitty_flags,
            } => {
                if let Some(t) = self.terms.get_mut(&session) {
                    t.apply_scrollback(base_seq, &data, kitty_flags);
                }
            }
            ServerEvent::Output { session, seq, data } => {
                if let Some(t) = self.terms.get_mut(&session) {
                    t.apply_output(seq, &data);
                    if let Some(text) = t.take_clipboard() {
                        crate::term::set_clipboard(&text);
                        self.toast(
                            ToastLevel::Info,
                            format!("copied {} bytes to the clipboard", text.len()),
                        );
                    }
                }
            }
            ServerEvent::KittyFlags { session, flags } => {
                if let Some(t) = self.terms.get_mut(&session) {
                    t.kitty_flags = flags;
                }
            }
            ServerEvent::Branches {
                project,
                local,
                remote,
            } => {
                if let Some(Overlay::Wizard(w)) = self.overlay.as_mut() {
                    if w.project == project {
                        w.local = local;
                        w.remote = remote;
                        w.loading_branches = false;
                    }
                }
            }
            ServerEvent::AskChoices { name, choices, .. } => {
                if let Some(Overlay::Loading { pending }) = self.overlay.take() {
                    if pending.current().map(|a| a.name == name).unwrap_or(false) {
                        self.open_ask_picker(pending, choices);
                    }
                }
            }
            ServerEvent::Focus { session } => self.focus_session(&session),
            ServerEvent::Toast { level, message } => self.toast(level, message),
            ServerEvent::Ok => {}
        }
    }

    fn notify_transition(&mut self, info: &SessionInfo) {
        let prev = self.seen_status.insert(info.id.clone(), info.status);
        if !self.global.ui.desktop_notifications || prev == Some(info.status) {
            return;
        }
        if !matches!(info.status, Status::Done | Status::Waiting) || !info.kind.is_agent() {
            return;
        }
        let visible = self.mode == Mode::Terminal
            && self.focused_session().as_deref() == Some(info.id.as_str());
        if visible {
            return;
        }
        let what = if info.status == Status::Done {
            "finished"
        } else {
            "needs you"
        };
        let branch = self
            .projects
            .iter()
            .flat_map(|p| p.worktrees.iter())
            .find(|w| w.path == info.worktree)
            .map(|w| w.label())
            .unwrap_or_default();
        let msg = format!("{} {what} — {branch}", info.kind.label());
        let script = format!(
            "display notification {:?} with title \"hive\" subtitle {:?}",
            msg, info.title
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    fn restore_ui(&mut self, state: Option<&str>) {
        let st: UiState = state
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        if st.expanded.is_empty() && st.selected.is_none() {
            // First run: expand every project.
            for p in &self.projects {
                self.expanded.insert(format!("p:{}", p.id));
            }
        } else {
            self.expanded = st.expanded.into_iter().collect();
        }
        self.active_tab = st
            .active_tabs
            .into_iter()
            .map(|(k, v)| (PathBuf::from(k), v))
            .collect();
        self.vscode_opened = st.vscode.iter().map(PathBuf::from).collect();
        if let Some(w) = st.sidebar_width {
            self.sidebar_width = w;
        }
        self.rebuild_rows();
        if let Some(k) = st.selected {
            self.select_key(&k);
        }
    }

    pub fn save_ui(&mut self, force: bool) {
        if !(self.ui_dirty && (force || self.last_ui_save.elapsed() > Duration::from_secs(5))) {
            return;
        }
        let st = UiState {
            expanded: self.expanded.iter().cloned().collect(),
            selected: self.selected_row().map(|r| self.row_key(&r)),
            active_tabs: self
                .active_tab
                .iter()
                .map(|(k, v)| (k.display().to_string(), v.clone()))
                .collect(),
            sidebar_width: Some(self.sidebar_width),
            vscode: self
                .vscode_opened
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        };
        if let Ok(s) = serde_json::to_string(&st) {
            self.send(ClientRequest::SaveUiState { state: s });
        }
        self.ui_dirty = false;
        self.last_ui_save = Instant::now();
    }

    // ------------------------------------------------------------ attach / resize

    /// Make the visible sessions attached and sized; detach the rest.
    pub fn sync_attachments(&mut self) {
        let visible: Vec<(String, Rect)> = self.pane_rects.clone();
        let want: HashSet<String> = visible.iter().map(|(id, _)| id.clone()).collect();
        for (id, rect) in &visible {
            let (rows, cols) = (rect.height.max(2), rect.width.max(2));
            let Some(info) = self.session(id).cloned() else {
                continue;
            };
            let term = self
                .terms
                .entry(id.clone())
                .or_insert_with(|| AttachedTerm::new(rows, cols));
            let resized = term.parser.screen().size() != (rows, cols);
            term.resize(rows, cols);
            self.term_lru.retain(|s| s != id);
            self.term_lru.push_back(id.clone());
            if !self.attached.contains(id) {
                let from_seq = if term.synced { term.next_seq } else { 0 };
                self.send(ClientRequest::Attach {
                    session: id.clone(),
                    from_seq,
                    cols,
                    rows,
                });
                self.attached.insert(id.clone());
            } else if resized && info.alive {
                self.send(ClientRequest::Resize {
                    session: id.clone(),
                    cols,
                    rows,
                });
            }
            if info.status == Status::Done && (self.mode == Mode::Terminal || visible.len() == 1) {
                self.send(ClientRequest::MarkSeen {
                    session: id.clone(),
                });
            }
        }
        let stale: Vec<String> = self
            .attached
            .iter()
            .filter(|id| !want.contains(*id))
            .cloned()
            .collect();
        for id in stale {
            self.send(ClientRequest::Detach {
                session: id.clone(),
            });
            self.attached.remove(&id);
        }
        // Evict old terminals beyond the cache budget.
        while self.term_lru.len() > TERM_CACHE_MAX {
            if let Some(old) = self.term_lru.pop_front() {
                if want.contains(&old) {
                    self.term_lru.push_back(old);
                    break;
                }
                self.terms.remove(&old);
            }
        }
    }

    fn pane_size(&self) -> (u16, u16) {
        (self.areas.pane.width.max(10), self.areas.pane.height.max(4))
    }

    // ------------------------------------------------------------ actions

    fn spawn(&mut self, kind: SessionKind) {
        let Some((p, w)) = self.selected_worktree() else {
            self.toast(ToastLevel::Warn, "add a project first (a)");
            return;
        };
        let (cols, rows) = self.pane_size();
        let req = SpawnSession {
            project: p.id.clone(),
            worktree: w.path.clone(),
            kind,
            cwd: self.scope_dir(),
            cols,
            rows,
        };
        self.send(ClientRequest::SpawnSession(req));
        self.mode = Mode::Terminal;
    }

    fn start_run_flow(&mut self, restart_active: bool) {
        let Some((p, w)) = self.selected_worktree() else {
            return;
        };
        let (pid, wpath) = (p.id.clone(), w.path.clone());
        if restart_active {
            if let Some(Tab::Group { target, .. }) = self.active_tab() {
                if let Some(run) = p.runs.iter().find(|r| r.name == target) {
                    let pending = PendingRun::new(pid, wpath, run);
                    return self.advance_run(pending);
                }
            }
        }
        match p.runs.len() {
            0 => self.toast(
                ToastLevel::Warn,
                "no run targets — add [[run]] to .hive.toml or ~/.config/hive/projects/<name>.toml",
            ),
            1 => {
                let pending = PendingRun::new(pid, wpath, &p.runs[0]);
                self.advance_run(pending);
            }
            _ => {
                let items = p
                    .runs
                    .iter()
                    .map(|r| {
                        let single = r.procs.len() == 1;
                        let ports: Vec<String> = r
                            .procs
                            .iter()
                            .filter_map(|pr| {
                                let port = pr.base_port? + w.slot * p.port_stride;
                                Some(if single {
                                    format!(":{port}")
                                } else {
                                    format!("{}:{port}", pr.name)
                                })
                            })
                            .collect();
                        PickItem {
                            label: r.name.clone(),
                            detail: ports.join("  "),
                        }
                    })
                    .collect();
                self.overlay = Some(Overlay::Picker(Picker::new(
                    "run",
                    items,
                    PickAction::Run {
                        project: pid,
                        worktree: wpath,
                    },
                )));
            }
        }
    }

    /// Ask the next question of a pending run, or start it.
    fn advance_run(&mut self, pending: PendingRun) {
        match pending.current() {
            None => {
                let (cols, rows) = self.pane_size();
                self.send(ClientRequest::StartRun(StartRun {
                    project: pending.project,
                    worktree: pending.worktree,
                    target: pending.target,
                    answers: pending.answers,
                    cols,
                    rows,
                }));
            }
            Some(ask) if ask.has_choices => {
                self.send(ClientRequest::AskChoices {
                    project: pending.project.clone(),
                    worktree: pending.worktree.clone(),
                    target: pending.target.clone(),
                    name: ask.name.clone(),
                });
                self.overlay = Some(Overlay::Loading { pending });
            }
            Some(ask) => {
                let mut input = TextInput::default();
                if let Some(d) = &ask.default {
                    input.set(d);
                }
                let title = ask.prompt.clone();
                self.overlay = Some(Overlay::Input {
                    title,
                    input,
                    action: InputAction::Ask(pending),
                });
            }
        }
    }

    fn open_ask_picker(&mut self, pending: PendingRun, choices: Vec<String>) {
        let ask = pending.current().cloned().unwrap();
        if choices.is_empty() {
            let mut input = TextInput::default();
            if let Some(d) = &ask.default {
                input.set(d);
            }
            self.overlay = Some(Overlay::Input {
                title: ask.prompt,
                input,
                action: InputAction::Ask(pending),
            });
            return;
        }
        let items: Vec<PickItem> = choices
            .into_iter()
            .map(|c| PickItem {
                label: c,
                detail: String::new(),
            })
            .collect();
        let mut picker = Picker::new(&ask.prompt, items, PickAction::AskChoice(pending));
        if let Some(d) = &ask.default {
            if let Some(i) = picker.items.iter().position(|it| &it.label == d) {
                picker.sel = picker.filtered().iter().position(|&x| x == i).unwrap_or(0);
            }
        }
        self.overlay = Some(Overlay::Picker(picker));
    }

    fn stop_active_run(&mut self) {
        let Some((p, w)) = self.selected_worktree() else {
            return;
        };
        if let Some(Tab::Group { target, .. }) = self.active_tab() {
            let req = ClientRequest::StopRun {
                project: p.id.clone(),
                worktree: w.path.clone(),
                target,
            };
            self.send(req);
        } else {
            self.toast(ToastLevel::Info, "select a run tab to stop it");
        }
    }

    fn close_active(&mut self) {
        let Some(tab) = self.active_tab() else { return };
        let alive = tab
            .sessions()
            .iter()
            .any(|id| self.session(id).map(|s| s.alive).unwrap_or(false));
        let label = match &tab {
            Tab::Single(id) => self
                .session(id)
                .map(|s| s.title.clone())
                .unwrap_or_default(),
            Tab::Group { target, .. } => format!("run:{target}"),
        };
        let action = Action::CloseSessions(tab.sessions());
        if alive {
            self.overlay = Some(Overlay::Confirm {
                msg: format!("Kill and close “{label}”?"),
                action,
            });
        } else {
            self.run_action(action);
        }
    }

    fn resume_active(&mut self) {
        let Some(id) = self.focused_session() else {
            return;
        };
        let Some(s) = self.session(&id).cloned() else {
            return;
        };
        if s.alive {
            return;
        }
        if s.group().is_some() {
            return self.start_run_flow(true);
        }
        if s.kind == SessionKind::Setup {
            return self.toast(ToastLevel::Info, "re-run setup with S");
        }
        let (cols, rows) = self.pane_size();
        self.send(ClientRequest::ResumeSession {
            session: id,
            cols,
            rows,
        });
        self.mode = Mode::Terminal;
    }

    fn new_worktree(&mut self) {
        let Some(p) = self.selected_project() else {
            return self.toast(ToastLevel::Warn, "add a project first (a)");
        };
        let w = Wizard::create(p);
        self.send(ClientRequest::ListBranches {
            project: p.id.clone(),
        });
        self.overlay = Some(Overlay::Wizard(w));
    }

    fn setup_worktree(&mut self) {
        let Some((p, w)) = self.selected_worktree() else {
            return;
        };
        if p.steps.is_empty() {
            return self.toast(
                ToastLevel::Info,
                "no [[setup_step]] configured for this project",
            );
        }
        let wiz = Wizard::setup(p, w);
        self.overlay = Some(Overlay::Wizard(wiz));
    }

    fn delete_worktree(&mut self) {
        let Some((p, w)) = self.selected_worktree() else {
            return;
        };
        if w.is_main
            || !matches!(
                self.selected_row(),
                Some(Row::Worktree(..)) | Some(Row::Package(..))
            )
        {
            return self.toast(ToastLevel::Warn, "select a (non-main) worktree to remove");
        }
        let n = self
            .sessions
            .iter()
            .filter(|s| s.worktree == w.path && s.alive)
            .count();
        let mut msg = format!(
            "Remove worktree “{}” ({})?",
            w.label(),
            hive_core::paths::tildify(&w.path)
        );
        if w.prunable {
            msg = format!(
                "“{}” points at a missing directory. Prune it from git?",
                w.label()
            );
        } else if w.dirty {
            msg.push_str("\nIt has uncommitted changes — they will be LOST (force).");
        }
        if n > 0 {
            msg.push_str(&format!("\n{n} running session(s) will be killed."));
        }
        let action = Action::RemoveWorktree {
            project: p.id.clone(),
            path: w.path.clone(),
            force: w.dirty,
        };
        self.overlay = Some(Overlay::Confirm { msg, action });
    }

    fn next_attention(&mut self) {
        let mut cands: Vec<&SessionInfo> = self
            .sessions
            .iter()
            .filter(|s| matches!(s.status, Status::Waiting | Status::Done))
            .collect();
        if cands.is_empty() {
            return self.toast(ToastLevel::Info, "nothing needs you");
        }
        cands.sort_by_key(|s| (std::cmp::Reverse(s.status), s.created_at));
        let cur = self.focused_session();
        let idx = cur
            .and_then(|c| cands.iter().position(|s| s.id == c))
            .map(|i| (i + 1) % cands.len())
            .unwrap_or(0);
        let id = cands[idx].id.clone();
        self.focus_session(&id);
    }

    fn open_palette(&mut self) {
        let mut items = Vec::new();
        let mut targets = Vec::new();
        for p in &self.projects {
            for w in &p.worktrees {
                items.push(PickItem {
                    label: format!("{} › {}", p.name, w.label()),
                    detail: hive_core::paths::tildify(&w.path),
                });
                targets.push(PaletteTarget::Worktree(w.path.clone()));
            }
        }
        for s in &self.sessions {
            let branch = self
                .projects
                .iter()
                .flat_map(|p| p.worktrees.iter())
                .find(|w| w.path == s.worktree)
                .map(|w| w.label())
                .unwrap_or_default();
            items.push(PickItem {
                label: format!("{} · {}", s.title, branch),
                detail: s.kind.label(),
            });
            targets.push(PaletteTarget::Session(s.id.clone()));
        }
        self.overlay = Some(Overlay::Picker(Picker::new(
            "jump to",
            items,
            PickAction::Palette(targets),
        )));
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::CloseSessions(ids) => {
                for id in ids {
                    self.send(ClientRequest::CloseSession { session: id });
                }
            }
            Action::RemoveWorktree {
                project,
                path,
                force,
            } => {
                self.send(ClientRequest::RemoveWorktree {
                    project,
                    path,
                    force,
                });
            }
            Action::RemoveProject(id) => self.send(ClientRequest::RemoveProject { project: id }),
            Action::QuitAndStop => {
                self.send(ClientRequest::Shutdown);
                self.quit = true;
            }
        }
    }

    // ------------------------------------------------------------ input

    pub fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        if self.overlay.is_some() {
            return self.overlay_key(key);
        }
        if self.mode == Mode::Terminal {
            return self.terminal_key(key);
        }
        self.nav_key(key);
    }

    fn terminal_key(&mut self, key: KeyEvent) {
        if matches_binding(&key, &self.unlock) {
            self.mode = Mode::Nav;
            return;
        }
        let Some(id) = self.focused_session() else {
            self.mode = Mode::Nav;
            return;
        };
        let alive = self.session(&id).map(|s| s.alive).unwrap_or(false);
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
        {
            let page = self.areas.pane.height.max(2) as isize - 1;
            if let Some(t) = self.terms.get_mut(&id) {
                t.scroll(if key.code == KeyCode::PageUp {
                    page
                } else {
                    -page
                });
            }
            return;
        }
        if !alive {
            if key.code == KeyCode::Enter {
                self.resume_active();
            }
            return;
        }
        let (flags, app_cursor) = self
            .terms
            .get(&id)
            .map(|t| (t.kitty_flags, t.parser.screen().application_cursor()))
            .unwrap_or((0, false));
        if let Some(bytes) = encode_key(&key, flags, app_cursor) {
            if let Some(t) = self.terms.get_mut(&id) {
                t.scroll_to_bottom();
            }
            self.send(ClientRequest::Input {
                session: id,
                data: bytes,
            });
        }
    }

    fn nav_key(&mut self, key: KeyEvent) {
        let key = self
            .nav_remap
            .iter()
            .find(|(b, _)| matches_exact(&key, b))
            .map(|(_, to)| *to)
            .unwrap_or(key);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('r') if ctrl => self.send(ClientRequest::ReloadConfig),
            KeyCode::Char('c') if ctrl => self.quit = true,
            // Other ctrl/alt chords (e.g. the unlock key pressed twice) must
            // never fall through to the plain-letter actions below.
            KeyCode::Char(_) if ctrl || alt => {}
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('g') | KeyCode::Home => self.sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.sel = self.rows.len().saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => self.set_expanded(true),
            KeyCode::Char('h') | KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Char(' ') => self.toggle_expanded(),
            KeyCode::Enter => {
                if let Some(id) = self.focused_session() {
                    if self.session(&id).map(|s| s.alive).unwrap_or(false) {
                        self.mode = Mode::Terminal;
                    } else {
                        self.resume_active();
                    }
                } else if matches!(self.selected_row(), Some(Row::Project(_))) {
                    self.toggle_expanded();
                } else {
                    self.spawn(SessionKind::Shell);
                }
            }
            KeyCode::Char('c') => self.spawn(SessionKind::Claude),
            KeyCode::Char('x') => self.spawn(SessionKind::Codex),
            KeyCode::Char('t') => self.spawn(SessionKind::Shell),
            KeyCode::Char('r') => self.start_run_flow(false),
            KeyCode::Char('R') => self.start_run_flow(true),
            KeyCode::Char('s') => self.stop_active_run(),
            KeyCode::Char('n') => self.new_worktree(),
            KeyCode::Char('S') => self.setup_worktree(),
            KeyCode::Char('D') => self.delete_worktree(),
            KeyCode::Char('a') => {
                let mut input = TextInput::default();
                if let Ok(cwd) = std::env::current_dir() {
                    input.set(&cwd.display().to_string());
                }
                self.overlay = Some(Overlay::Input {
                    title: "Add project (path to a git repo)".into(),
                    input,
                    action: InputAction::AddProject,
                });
            }
            KeyCode::Char('X') => {
                if let Some(Row::Project(pi)) = self.selected_row() {
                    let p = &self.projects[pi];
                    self.overlay = Some(Overlay::Confirm {
                        msg: format!("Remove project “{}” from hive? (files and worktrees are untouched; its sessions are killed)", p.name),
                        action: Action::RemoveProject(p.id.clone()),
                    });
                } else {
                    self.toast(ToastLevel::Info, "select a project row to remove it");
                }
            }
            KeyCode::Char('e') => {
                if let Some((p, w)) = self.selected_worktree() {
                    let path = w.path.clone();
                    self.send(ClientRequest::OpenVscode {
                        project: p.id.clone(),
                        worktree: path.clone(),
                    });
                    self.vscode_opened.insert(path);
                    self.ui_dirty = true;
                }
            }
            KeyCode::Char('w') => self.close_active(),
            KeyCode::Char('u') => self.resume_active(),
            KeyCode::Char('.') => self.next_attention(),
            KeyCode::Char('/') => self.open_palette(),
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
            KeyCode::Char('[') => self.cycle_group_focus(-1),
            KeyCode::Char(']') => self.cycle_group_focus(1),
            KeyCode::Tab => self.cycle_tab(1),
            KeyCode::BackTab => self.cycle_tab(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                let tabs = self.current_tabs();
                if let (Some(t), Some((_, w))) = (tabs.get(i), self.selected_worktree()) {
                    let path = w.path.clone();
                    self.set_active(path, t.key());
                }
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let page = self.areas.pane.height.max(2) as isize - 1;
                if let Some(id) = self.focused_session() {
                    if let Some(t) = self.terms.get_mut(&id) {
                        t.scroll(if key.code == KeyCode::PageUp {
                            page
                        } else {
                            -page
                        });
                    }
                }
            }
            KeyCode::Char('<') => {
                self.sidebar_width = self.sidebar_width.saturating_sub(2).max(20);
                self.ui_dirty = true;
            }
            KeyCode::Char('>') => {
                self.sidebar_width = (self.sidebar_width + 2).min(80);
                self.ui_dirty = true;
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('Q') => self.overlay = Some(Overlay::Confirm {
                msg: "Quit AND stop the daemon? Every agent, shell and dev server will be killed."
                    .into(),
                action: Action::QuitAndStop,
            }),
            _ => {}
        }
        self.ui_dirty = true;
    }

    fn move_sel(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as isize;
        self.sel = (self.sel as isize + d).clamp(0, n - 1) as usize;
    }

    fn expand_key(&self) -> Option<String> {
        match self.selected_row()? {
            Row::Project(pi) => Some(format!("p:{}", self.projects[pi].id)),
            Row::Worktree(pi, wi) => Some(format!(
                "w:{}",
                self.projects[pi].worktrees[wi].path.display()
            )),
            Row::Package(..) => None,
        }
    }

    fn set_expanded(&mut self, on: bool) {
        if let Some(k) = self.expand_key() {
            if on {
                self.expanded.insert(k);
            } else {
                self.expanded.remove(&k);
            }
            self.rebuild_rows();
        }
    }

    fn toggle_expanded(&mut self) {
        if let Some(k) = self.expand_key() {
            let on = !self.expanded.contains(&k);
            self.set_expanded(on);
        }
    }

    fn collapse_or_parent(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let key = self.expand_key();
        if let Some(k) = key.filter(|k| self.expanded.contains(k)) {
            self.expanded.remove(&k);
            self.rebuild_rows();
            return;
        }
        let parent = match row {
            Row::Package(pi, wi, _) => Some(Row::Worktree(pi, wi)),
            Row::Worktree(pi, _) => Some(Row::Project(pi)),
            Row::Project(_) => None,
        };
        if let Some(p) = parent {
            if let Some(i) = self.rows.iter().position(|r| *r == p) {
                self.sel = i;
            }
        }
    }

    pub fn on_paste(&mut self, text: String) {
        self.dirty = true;
        if let Some(o) = self.overlay.as_mut() {
            if let Some(input) = o.text_input() {
                input.insert_str(&text.replace(['\n', '\r'], " "));
                if let Overlay::Wizard(w) = o {
                    w.on_branch_changed();
                }
            }
            return;
        }
        if self.mode != Mode::Terminal {
            return;
        }
        let Some(id) = self.focused_session() else {
            return;
        };
        let bracketed = self
            .terms
            .get(&id)
            .map(|t| t.parser.screen().bracketed_paste())
            .unwrap_or(false);
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let data = if bracketed {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        self.send(ClientRequest::Input {
            session: id,
            data: data.into_bytes(),
        });
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if self.overlay.is_some() {
            return;
        }
        let pos = Position {
            x: ev.column,
            y: ev.row,
        };
        // Dragging the sidebar's right border resizes it.
        let border_x = self.areas.sidebar.x + self.areas.sidebar.width.saturating_sub(1);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left)
                if ev.column == border_x && self.areas.sidebar.contains(pos) =>
            {
                self.resizing = true;
                return;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing => {
                self.sidebar_width = (ev.column + 1).clamp(20, 80);
                self.ui_dirty = true;
                self.dirty = true;
                return;
            }
            MouseEventKind::Up(_) if self.resizing => {
                self.resizing = false;
                return;
            }
            _ => {}
        }
        // Terminal panes.
        if let Some((id, rect)) = self
            .pane_rects
            .iter()
            .find(|(_, r)| r.contains(pos))
            .cloned()
        {
            let (col, row) = (ev.column - rect.x, ev.row - rect.y);
            let (mode, sgr, alt) = match self.terms.get(&id) {
                Some(t) => {
                    let s = t.parser.screen();
                    (
                        s.mouse_protocol_mode(),
                        s.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr,
                        s.alternate_screen(),
                    )
                }
                None => (vt100::MouseProtocolMode::None, false, false),
            };
            let alive = self.session(&id).map(|s| s.alive).unwrap_or(false);
            let scrolled = self
                .terms
                .get(&id)
                .map(|t| t.parser.screen().scrollback() > 0)
                .unwrap_or(false);
            match ev.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let up = ev.kind == MouseEventKind::ScrollUp;
                    if alive && mode != vt100::MouseProtocolMode::None && !scrolled {
                        if let Some(b) = encode_mouse(&ev, col, row, sgr) {
                            self.send(ClientRequest::Input {
                                session: id,
                                data: b,
                            });
                        }
                    } else if alive && alt {
                        let arrow: &[u8] = if up {
                            b"\x1b[A\x1b[A\x1b[A"
                        } else {
                            b"\x1b[B\x1b[B\x1b[B"
                        };
                        self.send(ClientRequest::Input {
                            session: id,
                            data: arrow.to_vec(),
                        });
                    } else if let Some(t) = self.terms.get_mut(&id) {
                        t.scroll(if up { 3 } else { -3 });
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.focus_session(&id);
                    if alive {
                        self.mode = Mode::Terminal;
                    }
                    if alive && mode != vt100::MouseProtocolMode::None {
                        if let Some(b) = encode_mouse(&ev, col, row, sgr) {
                            self.send(ClientRequest::Input {
                                session: id,
                                data: b,
                            });
                        }
                    }
                }
                MouseEventKind::Up(_) | MouseEventKind::Drag(_) | MouseEventKind::Down(_) => {
                    let wants = match ev.kind {
                        MouseEventKind::Drag(_) => {
                            matches!(
                                mode,
                                vt100::MouseProtocolMode::ButtonMotion
                                    | vt100::MouseProtocolMode::AnyMotion
                            )
                        }
                        _ => matches!(
                            mode,
                            vt100::MouseProtocolMode::PressRelease
                                | vt100::MouseProtocolMode::ButtonMotion
                                | vt100::MouseProtocolMode::AnyMotion
                        ),
                    };
                    if alive && wants {
                        if let Some(b) = encode_mouse(&ev, col, row, sgr) {
                            self.send(ClientRequest::Input {
                                session: id,
                                data: b,
                            });
                        }
                    }
                }
                _ => return,
            }
            self.dirty = true;
            return;
        }
        if !matches!(
            ev.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
        ) {
            return;
        }
        // Tabs.
        if let Some((_, key)) = self.tab_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
            if let Some((_, w)) = self.selected_worktree() {
                let path = w.path.clone();
                self.set_active(path, key);
            }
            return;
        }
        // Tree.
        let tree = self.areas.tree;
        if tree.contains(pos) {
            match ev.kind {
                MouseEventKind::ScrollUp => self.move_sel(-1),
                MouseEventKind::ScrollDown => self.move_sel(1),
                _ => {
                    let offset = crate::ui::tree_offset(self, tree.height as usize);
                    let i = offset + (ev.row - tree.y) as usize;
                    if i < self.rows.len() {
                        if self.sel == i {
                            self.toggle_expanded();
                        }
                        self.sel = i;
                        self.mode = Mode::Nav;
                        self.ui_dirty = true;
                    }
                }
            }
            self.dirty = true;
        }
    }

    // ------------------------------------------------------------ overlays

    fn overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        let ctrl_q = matches_binding(&key, &self.unlock);
        if key.code == KeyCode::Esc || ctrl_q {
            return;
        }
        match overlay {
            Overlay::Help | Overlay::Loading { .. } => {}
            Overlay::Confirm { msg, action } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.run_action(action),
                KeyCode::Char('n') | KeyCode::Char('N') => {}
                _ => self.overlay = Some(Overlay::Confirm { msg, action }),
            },
            Overlay::Input {
                title,
                mut input,
                action,
            } => {
                if key.code == KeyCode::Enter {
                    let value = input.value.trim().to_string();
                    match action {
                        InputAction::AddProject => {
                            if !value.is_empty() {
                                let path = hive_core::paths::expand_tilde(&value);
                                self.send(ClientRequest::AddProject { path });
                            }
                        }
                        InputAction::Ask(mut pending) => {
                            pending.answer(value);
                            self.advance_run(pending);
                        }
                    }
                } else {
                    input.handle_key(&key);
                    self.overlay = Some(Overlay::Input {
                        title,
                        input,
                        action,
                    });
                }
            }
            Overlay::Picker(mut picker) => match key.code {
                KeyCode::Enter => {
                    let choice = picker.selected();
                    match (choice, picker.action) {
                        (Some(i), PickAction::Run { project, worktree }) => {
                            let p = self.projects.iter().find(|p| p.id == project);
                            if let Some(run) = p.and_then(|p| p.runs.get(i)) {
                                let pending = PendingRun::new(project, worktree, run);
                                self.advance_run(pending);
                            }
                        }
                        (Some(i), PickAction::AskChoice(mut pending)) => {
                            pending.answer(picker.items[i].label.clone());
                            self.advance_run(pending);
                        }
                        (None, PickAction::AskChoice(mut pending))
                            if !picker.filter.value.is_empty() =>
                        {
                            // Free text beats an empty match list.
                            pending.answer(picker.filter.value.clone());
                            self.advance_run(pending);
                        }
                        (Some(i), PickAction::Palette(targets)) => match &targets[i] {
                            PaletteTarget::Worktree(p) => {
                                let p = p.clone();
                                self.reveal_worktree(&p);
                            }
                            PaletteTarget::Session(id) => {
                                let id = id.clone();
                                self.focus_session(&id);
                            }
                        },
                        _ => {}
                    }
                }
                _ => {
                    picker.handle_key(&key);
                    self.overlay = Some(Overlay::Picker(picker));
                }
            },
            Overlay::Wizard(mut w) => {
                let submit = key.code == KeyCode::Enter && !w.enter_selects_suggestion();
                if submit {
                    match w.submit() {
                        Ok(req) => {
                            let req = match req {
                                WizardResult::Create(c) => ClientRequest::CreateWorktree(c),
                                WizardResult::Setup {
                                    project,
                                    worktree,
                                    steps,
                                    env,
                                } => ClientRequest::RunSetup {
                                    project,
                                    worktree,
                                    steps,
                                    env,
                                },
                            };
                            self.send(req);
                        }
                        Err(e) => {
                            w.error = Some(e);
                            self.overlay = Some(Overlay::Wizard(w));
                        }
                    }
                } else {
                    w.handle_key(&key);
                    self.overlay = Some(Overlay::Wizard(w));
                }
            }
        }
    }

    // ------------------------------------------------------------ rollups

    /// Most attention-worthy status among a worktree's sessions.
    /// (most urgent agent status, any run proc alive, any proc failed).
    pub fn worktree_status(&self, path: &Path) -> Option<(Status, bool, bool)> {
        let ss: Vec<&SessionInfo> = self
            .sessions
            .iter()
            .filter(|s| s.worktree == path)
            .collect();
        if ss.is_empty() {
            return None;
        }
        let st = ss
            .iter()
            .filter(|s| s.kind.is_agent())
            .map(|s| s.status)
            .max()
            .unwrap_or_default();
        let running = ss.iter().any(|s| s.alive && s.group().is_some());
        let failed = ss
            .iter()
            .any(|s| !s.alive && s.exit_code.map(|c| c != 0).unwrap_or(false));
        Some((st, running, failed))
    }

    pub fn project_status(&self, p: &ProjectInfo) -> Option<Status> {
        self.sessions
            .iter()
            .filter(|s| s.project == p.id && s.kind.is_agent())
            .map(|s| s.status)
            .max()
    }

    pub fn anything_working(&self) -> bool {
        self.sessions
            .iter()
            .any(|s| s.status == Status::Working && s.alive && s.kind.is_agent())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlock_in_nav_does_not_quit() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, GlobalConfig::default());
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit);
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(app.quit);
    }
}
